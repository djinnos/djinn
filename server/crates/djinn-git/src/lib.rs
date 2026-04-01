use std::path::{Path, PathBuf};

/// Lower CPU and I/O priority for a child process so djinn operations do not
/// starve interactive user applications (browser, editor, etc.).
///
/// Errors are intentionally ignored — some containers restrict these calls.
#[cfg(unix)]
fn lower_process_priority(cmd: &mut tokio::process::Command) {
    // SAFETY: pre_exec runs in the forked child before exec.
    // All calls here are async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            // Nice level 10 — below default 0, yields to user processes under contention.
            let _ = libc::setpriority(libc::PRIO_PROCESS, 0, 10);

            // I/O priority: best-effort class (2) with lowest priority (7).
            #[cfg(target_os = "linux")]
            {
                const IOPRIO_WHO_PROCESS: i32 = 1;
                const IOPRIO_CLASS_BE: i32 = 2;
                let ioprio_val = (IOPRIO_CLASS_BE << 13) | 7;
                let _ = libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, ioprio_val);
            }

            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn lower_process_priority(_cmd: &mut tokio::process::Command) {}

pub const PUSH_MAX_ATTEMPTS: u32 = 3;
pub const REBASE_MAX_ATTEMPTS: u32 = 3;

pub fn is_retryable_git_command_error(err: &GitError) -> bool {
    let GitError::CommandFailed { stderr, .. } = err else {
        return false;
    };
    let s = stderr.to_lowercase();
    [
        "cannot lock ref",
        "failed to lock",
        "another git process",
        "resource temporarily unavailable",
        "connection reset",
        "connection timed out",
        "timed out",
        "remote end hung up unexpectedly",
    ]
    .iter()
    .any(|needle| s.contains(needle))
}

/// Returns `true` when the error looks like a transient network / connectivity
/// problem that is worth retrying (as opposed to a permanent auth or ref error).
pub fn is_transient_network_error(err: &GitError) -> bool {
    if matches!(err, GitError::Timeout { .. }) {
        return true;
    }
    let GitError::CommandFailed { stderr, .. } = err else {
        return false;
    };
    let s = stderr.to_lowercase();
    [
        "connection closed by remote host",
        "broken pipe",
        "could not read from remote repository",
        "unable to access",
        "connection timed out",
        "connection refused",
        "could not resolve host",
        "ssl",
        "tls",
        "gnutls",
        "connection reset",
        "remote end hung up unexpectedly",
        "the remote end hung up unexpectedly",
        "early eof",
        "unexpected disconnect",
    ]
    .iter()
    .any(|needle| s.contains(needle))
}

pub fn is_non_fast_forward_error(err: &GitError) -> bool {
    let GitError::CommandFailed { stderr, .. } = err else {
        return false;
    };
    let s = stderr.to_lowercase();
    s.contains("non-fast-forward") || s.contains("fetch first") || s.contains("rejected")
}

pub fn retry_delay(attempt: u32) -> std::time::Duration {
    let exp = attempt.saturating_sub(1).min(4);
    let base_ms = 200u64.saturating_mul(1u64 << exp);
    let jitter_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_millis() as u64) % 151)
        .unwrap_or(0);
    std::time::Duration::from_millis(base_ms + jitter_ms)
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git2: {0}")]
    Git(#[from] git2::Error),

    #[error(
        "git command failed (exit {code}) in {cwd}: git {command}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    )]
    CommandFailed {
        code: i32,
        command: String,
        cwd: String,
        stdout: String,
        stderr: String,
    },

    #[error(
        "git commit rejected (exit {code}) in {cwd}: git {command}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    )]
    CommitRejected {
        code: i32,
        command: String,
        cwd: String,
        stdout: String,
        stderr: String,
    },

    #[error("merge conflict while squashing into {target_branch}: {files:?}")]
    MergeConflict {
        target_branch: String,
        files: Vec<String>,
    },

    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(#[from] anyhow::Error),

    #[error("actor channel closed")]
    ActorDead,

    #[error("no response from actor")]
    NoResponse,

    #[error("git command timed out after {timeout_secs}s in {cwd}: git {command}")]
    Timeout {
        timeout_secs: u64,
        command: String,
        cwd: String,
    },
}

pub mod actor;
pub use actor::{GitActorHandle, get_or_spawn};

#[derive(Debug, Clone)]
pub struct StatusSummary {
    pub staged: Vec<String>,
    pub modified: Vec<String>,
    pub untracked: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub sha: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

#[derive(Debug, Clone)]
pub struct MergeResult {
    pub commit_sha: String,
}

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: String,
}

pub async fn run_git_command(path: PathBuf, args: Vec<String>) -> Result<CommandOutput, GitError> {
    use std::process::Stdio;
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(&args)
        .current_dir(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    lower_process_priority(&mut cmd);
    let output = cmd.output().await?;

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        return Err(GitError::CommandFailed {
            code,
            command: args.join(" "),
            cwd: path.display().to_string(),
            stdout,
            stderr,
        });
    }

    Ok(CommandOutput {
        stdout,
        stderr,
        code,
    })
}

/// Like [`run_git_command`] but kills the child process if it does not complete
/// within `timeout`.  Returns [`GitError::Timeout`] on expiry.
pub async fn run_git_command_with_timeout(
    path: PathBuf,
    args: Vec<String>,
    timeout: std::time::Duration,
) -> Result<CommandOutput, GitError> {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;

    let mut cmd = tokio::process::Command::new("git");
    cmd.args(&args)
        .current_dir(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    lower_process_priority(&mut cmd);
    let mut child = cmd.spawn()?;

    // Take stdout/stderr handles so we can read them concurrently with wait,
    // avoiding deadlocks if the child fills a pipe buffer, while still being
    // able to kill the child on timeout.
    let mut stdout_handle = child.stdout.take();
    let mut stderr_handle = child.stderr.take();

    let io_future = async {
        let (status, stdout_buf, stderr_buf) = tokio::try_join!(
            child.wait(),
            async {
                let mut buf = Vec::new();
                if let Some(ref mut r) = stdout_handle {
                    r.read_to_end(&mut buf).await?;
                }
                Ok(buf)
            },
            async {
                let mut buf = Vec::new();
                if let Some(ref mut r) = stderr_handle {
                    r.read_to_end(&mut buf).await?;
                }
                Ok(buf)
            },
        )?;
        Ok::<_, std::io::Error>((status, stdout_buf, stderr_buf))
    };

    match tokio::time::timeout(timeout, io_future).await {
        Ok(Ok((status, stdout_buf, stderr_buf))) => {
            let code = status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&stdout_buf).into_owned();
            let stderr = String::from_utf8_lossy(&stderr_buf).into_owned();

            if !status.success() {
                return Err(GitError::CommandFailed {
                    code,
                    command: args.join(" "),
                    cwd: path.display().to_string(),
                    stdout,
                    stderr,
                });
            }

            Ok(CommandOutput {
                stdout,
                stderr,
                code,
            })
        }
        Ok(Err(io_err)) => Err(GitError::Io(io_err)),
        Err(_elapsed) => {
            // Timeout — the child is dropped here which sends SIGKILL.
            Err(GitError::Timeout {
                timeout_secs: timeout.as_secs(),
                command: args.join(" "),
                cwd: path.display().to_string(),
            })
        }
    }
}

pub async fn create_branch(
    path: PathBuf,
    short_id: String,
    target_branch: String,
) -> Result<(), GitError> {
    let branch_name = format!("task/{short_id}");
    let _ = run_git_command(
        path.clone(),
        vec!["fetch".into(), "origin".into(), target_branch.clone()],
    )
    .await;

    let remote_ref = format!("origin/{target_branch}");
    let create = run_git_command(
        path.clone(),
        vec!["branch".into(), branch_name.clone(), remote_ref],
    )
    .await;

    if create.is_err() {
        // Clean up any partial ref file left by the failed first attempt
        // before retrying with the local target branch.  The branch_name
        // contains a slash (task/xyz) so we need to join the components.
        let ref_path = path.join(".git/refs/heads").join(&branch_name);
        if ref_path.exists() {
            let _ = std::fs::remove_file(&ref_path);
        }
        run_git_command(
            path.clone(),
            vec!["branch".into(), branch_name.clone(), target_branch],
        )
        .await?;
    }

    // Verify the branch ref actually points to a valid commit.
    // A partial write (e.g. interrupted I/O, lock contention) can leave
    // a 0-byte ref file that git treats as broken.
    verify_branch_ref(&path, &branch_name).await?;

    Ok(())
}

/// Verify that a branch ref resolves to a valid commit object.
/// Returns an error if the ref is missing, empty, or corrupt.
async fn verify_branch_ref(path: &Path, branch: &str) -> Result<(), GitError> {
    let full_ref = format!("refs/heads/{branch}");
    run_git_command(
        path.to_path_buf(),
        vec![
            "rev-parse".into(),
            "--verify".into(),
            full_ref,
        ],
    )
    .await
    .map_err(|e| {
        // Clean up the broken ref so the next attempt starts fresh.
        let ref_path = path.join(".git/refs/heads").join(branch);
        if ref_path.exists() {
            let _ = std::fs::remove_file(&ref_path);
        }
        e
    })?;
    Ok(())
}

pub async fn delete_branch(path: PathBuf, branch: String) -> Result<(), GitError> {
    run_git_command(
        path.clone(),
        vec!["branch".into(), "-D".into(), branch.clone()],
    )
    .await?;

    let _ = run_git_command(
        path,
        vec!["push".into(), "origin".into(), "--delete".into(), branch],
    )
    .await;

    Ok(())
}

pub async fn create_worktree(
    path: PathBuf,
    task_short_id: String,
    branch: String,
    detach: bool,
) -> Result<PathBuf, GitError> {
    let _ = run_git_command(path.clone(), vec!["worktree".into(), "prune".into()]).await;

    let wt_path = path.join(".djinn").join("worktrees").join(&task_short_id);
    let mut args = vec!["worktree".into(), "add".into()];
    if detach {
        args.push("--detach".into());
    }
    args.push(wt_path.to_str().unwrap_or_default().into());
    args.push(branch);

    run_git_command(path, args).await?;
    Ok(wt_path)
}

pub async fn remove_worktree(path: PathBuf, wt_path: PathBuf) -> Result<(), GitError> {
    run_git_command(
        path.clone(),
        vec![
            "worktree".into(),
            "remove".into(),
            "--force".into(),
            wt_path.to_str().unwrap_or_default().into(),
        ],
    )
    .await?;

    let _ = run_git_command(path, vec!["worktree".into(), "prune".into()]).await;
    Ok(())
}

pub async fn list_worktrees(path: PathBuf) -> Result<Vec<WorktreeInfo>, GitError> {
    let out = run_git_command(
        path,
        vec!["worktree".into(), "list".into(), "--porcelain".into()],
    )
    .await?;

    let mut worktrees = Vec::new();
    let mut wt_path: Option<PathBuf> = None;
    let mut head: Option<String> = None;
    let mut branch: Option<String> = None;

    for line in out.stdout.lines() {
        if line.is_empty() {
            if let (Some(p), Some(h)) = (wt_path.take(), head.take()) {
                worktrees.push(WorktreeInfo {
                    path: p,
                    branch: branch.take(),
                    head: h,
                });
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            wt_path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            head = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("branch refs/heads/") {
            branch = Some(rest.to_string());
        }
    }

    if let (Some(p), Some(h)) = (wt_path, head) {
        worktrees.push(WorktreeInfo {
            path: p,
            branch: branch.take(),
            head: h,
        });
    }

    Ok(worktrees)
}

pub async fn unmerged_files(path: PathBuf) -> Result<Vec<String>, GitError> {
    let out = run_git_command(
        path,
        vec![
            "diff".into(),
            "--name-only".into(),
            "--diff-filter=U".into(),
        ],
    )
    .await?;
    Ok(out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

pub async fn squash_merge(
    path: PathBuf,
    branch: String,
    target_branch: String,
    message: String,
) -> Result<MergeResult, GitError> {
    const MERGE_PUSH_MAX_ATTEMPTS: u32 = 3;
    let mut last_error: Option<GitError> = None;

    for attempt in 1..=MERGE_PUSH_MAX_ATTEMPTS {
        let _ = run_git_command(
            path.clone(),
            vec!["fetch".into(), "origin".into(), target_branch.clone()],
        )
        .await;

        let origin_ref = format!("origin/{target_branch}");
        let rebase_wt_name = format!(
            ".rebase-{}-{}",
            branch.replace('/', "-"),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        let rebase_wt_path = path.join(".djinn").join("worktrees").join(&rebase_wt_name);
        let rebase_wt = rebase_wt_path.to_string_lossy().to_string();
        if run_git_command(
            path.clone(),
            vec![
                "worktree".into(),
                "add".into(),
                rebase_wt.clone(),
                branch.clone(),
            ],
        )
        .await
        .is_ok()
        {
            let rebase_ok = run_git_command(
                rebase_wt_path.clone(),
                vec!["rebase".into(), origin_ref.clone()],
            )
            .await
            .is_ok();
            if !rebase_ok {
                let _ = run_git_command(
                    rebase_wt_path.clone(),
                    vec!["rebase".into(), "--abort".into()],
                )
                .await;
            }
            let _ = run_git_command(
                path.clone(),
                vec![
                    "worktree".into(),
                    "remove".into(),
                    "--force".into(),
                    rebase_wt,
                ],
            )
            .await;
        }

        let temp_name = format!(
            ".merge-{}-{}",
            target_branch,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        let merge_wt_path = path.join(".djinn").join("worktrees").join(temp_name);
        let merge_wt = merge_wt_path.to_string_lossy().to_string();
        let origin_target = format!("origin/{target_branch}");

        run_git_command(
            path.clone(),
            vec![
                "worktree".into(),
                "add".into(),
                "--detach".into(),
                merge_wt.clone(),
                origin_target,
            ],
        )
        .await?;

        let merge_result = squash_merge_detached_worktree(
            path.clone(),
            merge_wt_path.clone(),
            branch.clone(),
            target_branch.clone(),
            message.clone(),
        )
        .await;

        let _ = run_git_command(
            path.clone(),
            vec![
                "worktree".into(),
                "remove".into(),
                "--force".into(),
                merge_wt,
            ],
        )
        .await;
        let _ = run_git_command(path.clone(), vec!["worktree".into(), "prune".into()]).await;

        match merge_result {
            Ok(result) => return Ok(result),
            Err(ref e) if attempt < MERGE_PUSH_MAX_ATTEMPTS && is_non_fast_forward_error(e) => {
                tracing::warn!(attempt, error = %e, target_branch = %target_branch, "push rejected");
                last_error = Some(merge_result.unwrap_err());
                tokio::time::sleep(retry_delay(attempt)).await;
                continue;
            }
            Err(e) => return Err(e),
        }
    }

    Err(last_error.unwrap_or_else(|| GitError::CommandFailed {
        code: 1,
        command: "squash_merge".into(),
        cwd: path.display().to_string(),
        stdout: String::new(),
        stderr: "exhausted merge-push retry attempts".into(),
    }))
}

pub async fn squash_merge_detached_worktree(
    repo_path: PathBuf,
    wt_path: PathBuf,
    branch: String,
    target_branch: String,
    message: String,
) -> Result<MergeResult, GitError> {
    if let Err(err) = run_git_command(
        wt_path.clone(),
        vec!["merge".into(), "--squash".into(), branch],
    )
    .await
    {
        if matches!(err, GitError::CommandFailed { .. }) {
            let files = unmerged_files(wt_path.clone()).await.unwrap_or_default();
            let _ = run_git_command(wt_path, vec!["merge".into(), "--abort".into()]).await;
            if !files.is_empty() {
                return Err(GitError::MergeConflict {
                    target_branch,
                    files,
                });
            }
        }
        return Err(err);
    }

    let staged = run_git_command(
        wt_path.clone(),
        vec!["diff".into(), "--cached".into(), "--name-only".into()],
    )
    .await?;
    if staged.stdout.trim().is_empty() {
        let out = run_git_command(wt_path.clone(), vec!["rev-parse".into(), "HEAD".into()]).await?;
        return Ok(MergeResult {
            commit_sha: out.stdout.trim().to_string(),
        });
    }

    match run_git_command(wt_path.clone(), vec!["commit".into(), "-m".into(), message]).await {
        Ok(_) => {}
        Err(GitError::CommandFailed {
            code,
            command,
            cwd,
            stdout,
            stderr,
        }) => {
            return Err(GitError::CommitRejected {
                code,
                command,
                cwd,
                stdout,
                stderr,
            });
        }
        Err(e) => return Err(e),
    }

    let out = run_git_command(wt_path.clone(), vec!["rev-parse".into(), "HEAD".into()]).await?;
    let commit_sha = out.stdout.trim().to_string();

    let push_refspec = format!("{commit_sha}:refs/heads/{target_branch}");
    let mut last_push_error: Option<GitError> = None;
    for attempt in 1..=PUSH_MAX_ATTEMPTS {
        match run_git_command(
            repo_path.clone(),
            vec!["push".into(), "origin".into(), push_refspec.clone()],
        )
        .await
        {
            Ok(_) => {
                last_push_error = None;
                break;
            }
            Err(e) if attempt < PUSH_MAX_ATTEMPTS && is_retryable_git_command_error(&e) => {
                last_push_error = Some(e);
                tokio::time::sleep(retry_delay(attempt)).await;
            }
            Err(e) => return Err(e),
        }
    }
    if let Some(e) = last_push_error {
        return Err(e);
    }

    Ok(MergeResult { commit_sha })
}

pub async fn rebase_with_retry(path: &Path, upstream: &str) -> Result<(), GitError> {
    let mut last_error: Option<GitError> = None;
    for attempt in 1..=REBASE_MAX_ATTEMPTS {
        match run_git_command(
            path.to_path_buf(),
            vec!["rebase".into(), upstream.to_string()],
        )
        .await
        {
            Ok(_) => {
                last_error = None;
                break;
            }
            Err(e) if attempt < REBASE_MAX_ATTEMPTS && is_retryable_git_command_error(&e) => {
                let _ =
                    run_git_command(path.to_path_buf(), vec!["rebase".into(), "--abort".into()])
                        .await;
                last_error = Some(e);
                tokio::time::sleep(retry_delay(attempt)).await;
            }
            Err(e) => {
                let _ =
                    run_git_command(path.to_path_buf(), vec!["rebase".into(), "--abort".into()])
                        .await;
                return Err(e);
            }
        }
    }
    if let Some(e) = last_error {
        return Err(e);
    }
    Ok(())
}

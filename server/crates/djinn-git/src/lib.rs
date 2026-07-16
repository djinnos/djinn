use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use djinn_core::clock::{Clock, SystemClock};

/// Synchronous counterpart to [`lower_process_priority`] for
/// `std::process::Command`. Errors are intentionally ignored.
#[cfg(unix)]
fn lower_process_priority_sync(cmd: &mut std::process::Command) {
    unsafe {
        cmd.pre_exec(|| {
            let _ = libc::setpriority(libc::PRIO_PROCESS, 0, 10);
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
fn lower_process_priority_sync(_cmd: &mut std::process::Command) {}

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

/// Set GIT_CONFIG_COUNT=1 + GIT_CONFIG_KEY_0/VALUE_0 on a `std::process::Command`
/// so git accepts any repo regardless of cross-UID ownership. Env vars (not
/// `-c` flag) so that inner git subprocesses spawned by `git clone --local`
/// also inherit the rule.
fn apply_safe_directory_env_std(cmd: &mut std::process::Command) {
    cmd.env("GIT_CONFIG_COUNT", "1");
    cmd.env("GIT_CONFIG_KEY_0", "safe.directory");
    cmd.env("GIT_CONFIG_VALUE_0", "*");
}

/// Set GIT_CONFIG_COUNT=1 + GIT_CONFIG_KEY_0/VALUE_0 on a tokio `Command` so git
/// accepts any repo regardless of cross-UID ownership. Env vars (not `-c` flag)
/// so that inner git subprocesses spawned by `git clone --local` also inherit
/// the rule.
pub fn apply_safe_directory_env(cmd: &mut tokio::process::Command) {
    cmd.env("GIT_CONFIG_COUNT", "1");
    cmd.env("GIT_CONFIG_KEY_0", "safe.directory");
    cmd.env("GIT_CONFIG_VALUE_0", "*");
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
    let jitter_ms = SystemClock::new()
        .now()
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

pub mod submission_diff;
pub use submission_diff::{
    DEFAULT_SUBMISSION_BASE_REF, SubmissionDiffDigest, SubmissionDiffFingerprint,
    SubmissionDiffFingerprintConfig, SubmissionNoDiff, compute_submission_diff_fingerprint,
    compute_submission_diff_fingerprint_with_config,
};

pub mod verification_input;
pub use verification_input::{
    DEFAULT_VERIFICATION_BASE_REF, ResolvedExternalInputV1,
    VERIFICATION_INPUT_FINGERPRINT_VERSION_V1, VerificationInputDigestV1, VerificationInputError,
    VerificationInputFingerprint, VerificationInputFingerprintConfig, VerificationInputUnavailable,
    compute_verification_input_fingerprint, compute_verification_input_fingerprint_with_config,
};

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod lib_tests;

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

impl CommandOutput {
    /// True when the recorded exit code represents a successful exit (code 0).
    pub fn is_success(&self) -> bool {
        self.code == 0
    }

    /// Returns the recorded exit code as an `Option<i32>`, mirroring
    /// `std::process::ExitStatus::code()`.
    pub fn exit_code(&self) -> Option<i32> {
        Some(self.code)
    }
}

/// Raw binary output from a git command, returned when the output may contain
/// NUL-delimited or non-UTF-8 bytes and must not pass through lossy conversion.
#[derive(Debug, Clone)]
pub struct BinaryCommandOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: i32,
}

impl BinaryCommandOutput {
    /// True when the recorded exit code represents a successful exit (code 0).
    pub fn is_success(&self) -> bool {
        self.code == 0
    }
}

#[derive(Debug, Clone)]
pub struct MergeResult {
    pub commit_sha: String,
}

pub async fn run_git_command(path: PathBuf, args: Vec<String>) -> Result<CommandOutput, GitError> {
    use std::process::Stdio;
    let mut cmd = tokio::process::Command::new("git");
    // Inject `safe.directory=*` via env vars so git trusts any path we
    // hand it — needed because the K8s worker Pod runs as a different
    // UID than the user that owns the shared /mirror PVC, and git
    // 2.35.2+ rejects mixed-UID repos by default with
    //   `fatal: detected dubious ownership in repository at '<path>'`.
    // We use the env-var form (GIT_CONFIG_COUNT=1 + GIT_CONFIG_KEY_0 +
    // GIT_CONFIG_VALUE_0) rather than `-c safe.directory=*` because
    // `git clone --local` spawns an inner git subprocess against the
    // source repo that does NOT inherit the outer git's -c flags, only
    // env vars. Cheap on the host (just adds the rule to in-memory
    // config for this invocation chain) and avoids per-Pod
    // GIT_CONFIG_* wiring at the K8s manifest level.
    apply_safe_directory_env(&mut cmd);
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
    // Same safe.directory injection as run_git_command — see docs there.
    apply_safe_directory_env(&mut cmd);
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
        vec!["rev-parse".into(), "--verify".into(), full_ref],
    )
    .await
    .inspect_err(|_| {
        // Clean up the broken ref so the next attempt starts fresh.
        let ref_path = path.join(".git/refs/heads").join(branch);
        if ref_path.exists() {
            let _ = std::fs::remove_file(&ref_path);
        }
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

// ─── Borrowed-cwd helpers ────────────────────────────────────────────────────
//
// These are the same as `run_git_command[_with_timeout]` but take `&Path` so
// callers that already hold a borrow (e.g. a `Path` extracted from a project
// root, an `IndexTreeHandle`'s `&Path` field) don't have to clone a `PathBuf`
// just to satisfy the original signature.  They exist specifically to keep
// `djinn-graph` (and other callers in fztz Wave 1) on the djinn-git owner
// crate without forcing every call site to take a `PathBuf` by value.
//
// Both helpers apply the same `safe.directory=*` env injection and
// process-priority lowering as `run_git_command`.  The `&Path`→`PathBuf`
// step is just `to_path_buf()` and does not change filesystem semantics.

/// Like [`run_git_command`] but takes `&Path` so callers holding a borrow
/// don't need to clone a `PathBuf`.
pub async fn run_git_command_in(cwd: &Path, args: Vec<String>) -> Result<CommandOutput, GitError> {
    run_git_command(cwd.to_path_buf(), args).await
}

/// Like [`run_git_command_with_timeout`] but takes `&Path`.
pub async fn run_git_command_with_timeout_in(
    cwd: &Path,
    args: Vec<String>,
    timeout: std::time::Duration,
) -> Result<CommandOutput, GitError> {
    run_git_command_with_timeout(cwd.to_path_buf(), args, timeout).await
}

/// Like [`run_git_command`] but returns the [`CommandOutput`] even when git
/// exits non-zero. Only returns `Err` on a spawn / I/O failure; a non-zero
/// exit is reported through [`CommandOutput::code`] /
/// [`CommandOutput::is_success`] / [`CommandOutput::exit_code`].
///
/// Use this for git commands where a specific non-zero exit is an *expected
/// answer* rather than an error — e.g. `git merge-base --is-ancestor` (exit 1 =
/// "not an ancestor") or `git merge --no-commit` (exit 1 = merge conflict).
/// For commands where any non-zero exit is a genuine failure, prefer
/// [`run_git_command`] / [`run_git_command_in`] which surface it as
/// [`GitError::CommandFailed`].
pub async fn run_git_command_allow_failure(
    path: PathBuf,
    args: Vec<String>,
) -> Result<CommandOutput, GitError> {
    use std::process::Stdio;
    let mut cmd = tokio::process::Command::new("git");
    apply_safe_directory_env(&mut cmd);
    cmd.args(&args)
        .current_dir(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    lower_process_priority(&mut cmd);
    let output = cmd.output().await?;
    Ok(CommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code().unwrap_or(-1),
    })
}

/// Like [`run_git_command_allow_failure`] but takes `&Path`.
pub async fn run_git_command_in_allow_failure(
    cwd: &Path,
    args: Vec<String>,
) -> Result<CommandOutput, GitError> {
    run_git_command_allow_failure(cwd.to_path_buf(), args).await
}

/// Like [`run_git_command_in_allow_failure`] but merges additional environment
/// variables into the child process (e.g. `GIT_AUTHOR_NAME` for a `git merge`).
/// Returns the [`CommandOutput`] even when git exits non-zero; only returns
/// `Err` on a spawn / I/O failure.
pub async fn run_git_command_in_with_env_allow_failure(
    cwd: &Path,
    args: Vec<String>,
    extra_env: Vec<(String, String)>,
) -> Result<CommandOutput, GitError> {
    use std::process::Stdio;
    let mut cmd = tokio::process::Command::new("git");
    apply_safe_directory_env(&mut cmd);
    for (k, v) in &extra_env {
        cmd.env(k, v);
    }
    cmd.args(&args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    lower_process_priority(&mut cmd);
    let output = cmd.output().await?;
    Ok(CommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code().unwrap_or(-1),
    })
}
/// Like [`run_git_command_in`] but synchronous and returns the raw
/// `std::process::Output` (including stdout/stderr as `Vec<u8>`) so callers
/// that need binary/NUL-delimited output are not forced through lossy UTF-8
/// conversion. Used by the workspace mtime normalization path, which runs
/// inside `spawn_blocking`.
pub fn run_git_command_binary_in(
    cwd: &Path,
    args: Vec<String>,
) -> Result<BinaryCommandOutput, GitError> {
    use std::process::Stdio;
    let mut cmd = std::process::Command::new("git");
    apply_safe_directory_env_std(&mut cmd);
    cmd.args(&args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    lower_process_priority_sync(&mut cmd);
    let output = cmd.output()?;
    let code = output.status.code().unwrap_or(-1);
    if !output.status.success() {
        return Err(GitError::CommandFailed {
            code,
            command: args.join(" "),
            cwd: cwd.display().to_string(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(BinaryCommandOutput {
        stdout: output.stdout,
        stderr: output.stderr,
        code,
    })
}

/// Like [`run_git_command_in`] but accepts additional environment variables
/// that are merged into the child process environment (alongside the standard
/// `safe.directory=*` injection and priority lowering).
///
/// Useful for callers that need per-invocation overrides such as custom
/// `GIT_AUTHOR_NAME` / `GIT_COMMITTER_NAME` for commit operations.
pub async fn run_git_command_in_with_env(
    cwd: &Path,
    args: Vec<String>,
    extra_env: Vec<(String, String)>,
) -> Result<CommandOutput, GitError> {
    use std::process::Stdio;
    let mut cmd = tokio::process::Command::new("git");
    apply_safe_directory_env(&mut cmd);
    for (k, v) in &extra_env {
        cmd.env(k, v);
    }
    cmd.args(&args)
        .current_dir(cwd)
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
            cwd: cwd.display().to_string(),
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

/// `git rev-parse HEAD` against `repo_root`. Returns the trimmed SHA on
/// stdout. Used by callers that want the HEAD commit SHA without having to
/// manually parse `CommandOutput`.
pub async fn head_commit_sha(repo_root: &Path) -> Result<String, GitError> {
    let out = run_git_command(
        repo_root.to_path_buf(),
        vec!["rev-parse".into(), "HEAD".into()],
    )
    .await?;
    Ok(out.stdout.trim().to_string())
}

/// `git rev-list --count <range>` against `repo_root`. Returns the parsed
/// count. Used by callers that want the number of commits in a range without
/// manually parsing `CommandOutput`.
///
/// Errors out if git exits non-zero OR stdout isn't a valid u64.
pub async fn rev_list_count(repo_root: &Path, range: &str) -> Result<u64, GitError> {
    let out = run_git_command(
        repo_root.to_path_buf(),
        vec!["rev-list".into(), "--count".into(), range.to_string()],
    )
    .await?;
    out.stdout
        .trim()
        .parse::<u64>()
        .map_err(|e| GitError::Other(anyhow::anyhow!("rev-list count not a u64: {e}")))
}

/// `git rev-parse <rev>` against `repo_root`. Returns the trimmed output
/// (typically a 40-char commit SHA). Used by callers that need to resolve an
/// arbitrary revision without manually parsing `CommandOutput`.
pub async fn rev_parse(repo_root: &Path, rev: &str) -> Result<String, GitError> {
    let out = run_git_command(
        repo_root.to_path_buf(),
        vec!["rev-parse".into(), rev.to_string()],
    )
    .await?;
    Ok(out.stdout.trim().to_string())
}

/// Resolve the checked-out local HEAD without invoking the git executable.
///
/// This is intentionally limited to the repository already present on disk;
/// callers that need another revision must use the async command helpers.
pub fn head_sha(repo_root: &Path) -> Result<String, GitError> {
    let repository = git2::Repository::open(repo_root)?;
    Ok(repository.head()?.peel_to_commit()?.id().to_string())
}

/// `git merge-base <a> <b>` against `repo_root`. Returns the trimmed merge-base
/// SHA on stdout. Used by callers that need the common ancestor of two commits
/// without manually parsing `CommandOutput`.
pub async fn merge_base(repo_root: &Path, a: &str, b: &str) -> Result<String, GitError> {
    let out = run_git_command(
        repo_root.to_path_buf(),
        vec!["merge-base".into(), a.to_string(), b.to_string()],
    )
    .await?;
    Ok(out.stdout.trim().to_string())
}

// ─── Synchronous git2 helpers ─────────────────────────────────────────────
//
// These wrap libgit2 operations that callers (djinn-coordinator, djinn-stack)
// need synchronously.  They live in the djinn-git owner crate so non-owner
// crates never depend on `git2` directly.

/// One blob entry from a HEAD tree walk: relative path + size in bytes.
#[derive(Debug, Clone)]
pub struct HeadBlobEntry {
    pub path: String,
    pub size: u64,
}

/// Enumerate every blob in the current HEAD tree, returning its relative path
/// and size.  Works on both bare mirrors and working-tree repos.
///
/// Returns `Ok(vec![])` when HEAD is unresolvable (e.g. fresh repo with no
/// commits) — callers typically fall back to a filesystem walk in that case.
pub fn head_blob_list(root: &Path) -> Result<Vec<HeadBlobEntry>, GitError> {
    let repo = git2::Repository::open(root).or_else(|_| git2::Repository::open_bare(root))?;
    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => return Ok(Vec::new()),
    };
    let tree = head.peel_to_tree()?;

    let mut out = Vec::new();
    tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
        let name = match entry.name() {
            Some(n) => n,
            None => return git2::TreeWalkResult::Ok,
        };
        if entry.kind() == Some(git2::ObjectType::Blob) {
            let full = if dir.is_empty() {
                name.to_string()
            } else {
                format!("{dir}{name}")
            };
            let size = repo
                .find_blob(entry.id())
                .ok()
                .map(|b| b.size() as u64)
                .unwrap_or(0);
            out.push(HeadBlobEntry { path: full, size });
        }
        git2::TreeWalkResult::Ok
    })?;
    Ok(out)
}

/// Read the blob content for every path in `wanted` that exists in the HEAD
/// tree.  Returns a map of `path → UTF-8 body`.  Paths not found in the tree
/// are silently omitted.  Works on both bare mirrors and working-tree repos.
pub fn head_blob_bodies(
    root: &Path,
    wanted: &std::collections::BTreeSet<&str>,
) -> Result<std::collections::HashMap<String, String>, GitError> {
    let repo = git2::Repository::open(root).or_else(|_| git2::Repository::open_bare(root))?;
    let tree = repo.head()?.peel_to_tree()?;
    let mut out = std::collections::HashMap::new();
    tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
        if entry.kind() != Some(git2::ObjectType::Blob) {
            return git2::TreeWalkResult::Ok;
        }
        let name = match entry.name() {
            Some(n) => n,
            None => return git2::TreeWalkResult::Ok,
        };
        let full = if dir.is_empty() {
            name.to_string()
        } else {
            format!("{dir}{name}")
        };
        if !wanted.contains(full.as_str()) {
            return git2::TreeWalkResult::Ok;
        }
        if let Ok(blob) = repo.find_blob(entry.id())
            && let Ok(body) = std::str::from_utf8(blob.content())
        {
            out.insert(full, body.to_string());
        }
        git2::TreeWalkResult::Ok
    })?;
    Ok(out)
}

/// Returns `true` when the git repository at `path` has any uncommitted
/// changes (staged, modified, untracked, renamed, or deleted files).
///
/// Conservatively returns `false` if the path is not a valid git repo or
/// if the status probe fails, matching the existing coordinator behaviour
/// that treats errors as "not dirty" so we never promote a task to the PR
/// flow on a bogus signal.
pub fn worktree_is_dirty(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let repo = match git2::Repository::open(path) {
        Ok(repo) => repo,
        Err(_) => return false,
    };
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .include_ignored(false)
        .recurse_untracked_dirs(true);
    match repo.statuses(Some(&mut opts)) {
        Ok(statuses) => statuses.iter().any(|entry| {
            let s = entry.status();
            s.intersects(
                git2::Status::INDEX_NEW
                    | git2::Status::INDEX_MODIFIED
                    | git2::Status::INDEX_DELETED
                    | git2::Status::INDEX_RENAMED
                    | git2::Status::INDEX_TYPECHANGE
                    | git2::Status::WT_NEW
                    | git2::Status::WT_MODIFIED
                    | git2::Status::WT_DELETED
                    | git2::Status::WT_TYPECHANGE
                    | git2::Status::WT_RENAMED,
            )
        }),
        Err(_) => false,
    }
}

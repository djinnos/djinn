//! Fail-closed launcher for reusable final verification on Linux.
//!
//! This module intentionally does not use [`crate::SANDBOX`] or the agent-shell
//! policy.  Those APIs may select a heuristic fallback and grant broad host
//! reads/writes, neither of which is acceptable evidence for reusable
//! verification.

#![cfg(target_os = "linux")]

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use landlock::{
    ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
};

/// An argv invocation and the complete set of host paths it may use.
///
/// `tool_runtime` must include the executable and every runtime directory it
/// needs (for example `/usr`, `/bin`, `/lib`, and `/lib64`).  `output_directories`
/// are relative to `worktree`; every one must be absent before launch and is
/// created empty immediately before the child is spawned.
#[derive(Debug, Clone)]
pub struct FinalVerificationRequest {
    pub argv: Vec<String>,
    pub worktree: PathBuf,
    pub working_directory: PathBuf,
    pub tool_runtime: Vec<PathBuf>,
    pub read_only_external_mounts: Vec<PathBuf>,
    pub output_directories: Vec<PathBuf>,
    pub environment: BTreeMap<String, String>,
}

/// Observed child status and captured output.  A non-success status is command
/// evidence, rather than a successful verification result.
#[derive(Debug)]
pub struct FinalVerificationResult {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl FinalVerificationResult {
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// A policy violation detected before a child is permitted to execute.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FinalVerificationViolation {
    #[error("argv must contain a non-empty executable and no NUL bytes")]
    InvalidArgv,
    #[error("{field} is not an existing directory: {path}")]
    InvalidDirectory { field: &'static str, path: PathBuf },
    #[error("working directory escapes the worktree: {path}")]
    WorkingDirectoryOutsideWorktree { path: PathBuf },
    #[error("output directory must be a non-empty relative worktree path: {path}")]
    InvalidOutputDirectory { path: PathBuf },
    #[error("output-only directory was present before launch: {path}")]
    OutputOnlyPreexisting { path: PathBuf },
    #[error("output directories overlap: {first} and {second}")]
    OverlappingOutputDirectories { first: PathBuf, second: PathBuf },
}

/// A kernel-enforced access denial observed while the child was executing.
///
/// The launcher observes the child's syscall results with `ptrace`; this is an
/// enforcement-side signal, not a diagnostic supplied by the command. This
/// makes a violation impossible to forge by printing an error and impossible
/// to suppress by redirecting stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FinalVerificationRuntimeViolation {
    #[error("undeclared filesystem access was denied")]
    FilesystemAccessDenied,
    #[error("network access was denied")]
    NetworkAccessDenied,
}

/// Launcher failures are typed so callers can mark attempts ineligible without
/// treating a weaker execution as reusable evidence.
#[derive(Debug, thiserror::Error)]
pub enum FinalVerificationError {
    #[error("final-verification isolation backend is unavailable: {reason}")]
    BackendUnavailable { reason: &'static str },
    #[error("final-verification sandbox violation: {0}")]
    Violation(#[from] FinalVerificationViolation),
    #[error("final-verification runtime sandbox violation: {violation}")]
    RuntimeViolation {
        violation: FinalVerificationRuntimeViolation,
        /// Captured output lets callers audit the failed command without
        /// treating it as reusable verification evidence.
        result: FinalVerificationResult,
    },
    #[error("failed to prepare output directory {path}: {source}")]
    OutputPreparation { path: PathBuf, source: io::Error },
    #[error("failed to launch isolated final verification: {0}")]
    Launch(#[source] io::Error),
    #[error("failed to observe isolated final verification: {0}")]
    Observation(#[source] io::Error),
}

/// Execute a final-verification command under the strict reusable policy.
///
/// Availability is checked before filesystem mutation.  There is deliberately
/// no fallback path: lack of Landlock or an isolated network namespace returns
/// [`FinalVerificationError::BackendUnavailable`]. Kernel-enforced runtime
/// denials return [`FinalVerificationError::RuntimeViolation`] based on an
/// observed denied syscall, while ordinary command failures remain
/// [`FinalVerificationResult`] values.
pub fn launch_final_verification(
    request: FinalVerificationRequest,
) -> Result<FinalVerificationResult, FinalVerificationError> {
    let prepared = PreparedRequest::new(request)?;
    // Request validation performs no mutation, so report a pre-existing output
    // as a policy violation even if the host cannot provide the backend.
    ensure_backend_available()?;
    prepared.create_empty_output_directories()?;

    let mut command = Command::new(&prepared.argv[0]);
    command
        .args(&prepared.argv[1..])
        .current_dir(&prepared.working_directory)
        .env_clear()
        .envs(&prepared.environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let worktree = prepared.worktree;
    let runtimes = prepared.tool_runtime;
    let externals = prepared.read_only_external_mounts;
    let outputs = prepared.output_directories;
    // SAFETY: the closure only installs an already-described kernel policy.
    // All canonicalization and directory creation was completed in the parent.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(move || {
            enter_isolated_network_namespace()?;
            apply_filesystem_policy(&worktree, &runtimes, &externals, &outputs)
                .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))?;
            begin_syscall_observation()
        });
    }

    let mut child = command.spawn().map_err(FinalVerificationError::Launch)?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let stdout_reader = std::thread::spawn(move || read_pipe(stdout));
    let stderr_reader = std::thread::spawn(move || read_pipe(stderr));
    let (status, runtime_violation) = observe_child(child.id() as libc::pid_t)?;
    let result = FinalVerificationResult {
        exit_code: exit_code(status),
        stdout: stdout_reader
            .join()
            .map_err(|_| FinalVerificationError::Observation(io::Error::other("stdout reader panicked")))??,
        stderr: stderr_reader
            .join()
            .map_err(|_| FinalVerificationError::Observation(io::Error::other("stderr reader panicked")))??,
    };
    if let Some(violation) = runtime_violation {
        return Err(FinalVerificationError::RuntimeViolation { violation, result });
    }
    Ok(result)
}

fn read_pipe(mut pipe: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn begin_syscall_observation() -> io::Result<()> {
    if unsafe {
        libc::ptrace(
            libc::PTRACE_TRACEME,
            0,
            std::ptr::null_mut::<libc::c_void>(),
            std::ptr::null_mut::<libc::c_void>(),
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::raise(libc::SIGSTOP) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn exit_code(status: libc::c_int) -> Option<i32> {
    libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status))
}

#[cfg(target_arch = "x86_64")]
fn observe_child(
    root: libc::pid_t,
) -> Result<(libc::c_int, Option<FinalVerificationRuntimeViolation>), FinalVerificationError> {
    let mut status = 0;
    if unsafe { libc::waitpid(root, &mut status, libc::WUNTRACED) } != root || !libc::WIFSTOPPED(status) {
        return Err(FinalVerificationError::Observation(io::Error::last_os_error()));
    }
    let options = libc::PTRACE_O_TRACESYSGOOD
        | libc::PTRACE_O_TRACEFORK
        | libc::PTRACE_O_TRACEVFORK
        | libc::PTRACE_O_TRACECLONE;
    ptrace(root, libc::PTRACE_SETOPTIONS, 0, options as usize)?;
    ptrace(root, libc::PTRACE_SYSCALL, 0, 0)?;

    let mut tracees = HashMap::from([(root, None)]);
    let mut root_status = None;
    let mut violation = None;
    while !tracees.is_empty() {
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::__WALL) };
        if pid < 0 {
            return Err(FinalVerificationError::Observation(io::Error::last_os_error()));
        }
        if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
            tracees.remove(&pid);
            if pid == root {
                root_status = Some(status);
            }
            continue;
        }
        if !libc::WIFSTOPPED(status) {
            continue;
        }
        let signal = libc::WSTOPSIG(status);
        let event = (status as u32) >> 16;
        if matches!(event, libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK | libc::PTRACE_EVENT_CLONE) {
            let mut child = 0usize;
            ptrace(pid, libc::PTRACE_GETEVENTMSG, 0, &mut child as *mut usize as usize)?;
            tracees.entry(child as libc::pid_t).or_insert(None);
        }
        if signal == (libc::SIGTRAP | 0x80) {
            let entry = tracees.get_mut(&pid).expect("traced process must be registered");
            let number = syscall_number(pid)?;
            if let Some(syscall) = *entry {
                if let Some(found) = denied_syscall_violation(syscall, syscall_result(pid)?) {
                    violation.get_or_insert(found);
                }
                *entry = None;
            } else {
                *entry = Some(number);
            }
        }
        let deliver = if signal == libc::SIGTRAP || signal == libc::SIGSTOP || signal == (libc::SIGTRAP | 0x80) { 0 } else { signal };
        ptrace(pid, libc::PTRACE_SYSCALL, 0, deliver as usize)?;
    }
    Ok((root_status.unwrap_or(status), violation))
}

#[cfg(not(target_arch = "x86_64"))]
fn observe_child(
    _: libc::pid_t,
) -> Result<(libc::c_int, Option<FinalVerificationRuntimeViolation>), FinalVerificationError> {
    Err(FinalVerificationError::Observation(io::Error::new(
        io::ErrorKind::Unsupported,
        "syscall observation requires x86_64",
    )))
}

#[cfg(target_arch = "x86_64")]
fn ptrace(pid: libc::pid_t, request: libc::c_uint, address: usize, data: usize) -> io::Result<()> {
    if unsafe { libc::ptrace(request, pid, address as *mut libc::c_void, data as *mut libc::c_void) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_arch = "x86_64")]
fn syscall_number(pid: libc::pid_t) -> io::Result<i64> {
    let mut registers: libc::user_regs_struct = unsafe { std::mem::zeroed() };
    ptrace(pid, libc::PTRACE_GETREGS, &mut registers as *mut _ as usize, 0)?;
    Ok(registers.orig_rax as i64)
}

#[cfg(target_arch = "x86_64")]
fn syscall_result(pid: libc::pid_t) -> io::Result<i64> {
    let mut registers: libc::user_regs_struct = unsafe { std::mem::zeroed() };
    ptrace(pid, libc::PTRACE_GETREGS, &mut registers as *mut _ as usize, 0)?;
    Ok(registers.rax as i64)
}

#[cfg(target_arch = "x86_64")]
fn denied_syscall_violation(syscall: i64, result: i64) -> Option<FinalVerificationRuntimeViolation> {
    let errno = (-result) as libc::c_int;
    if [libc::ENETUNREACH, libc::EHOSTUNREACH, libc::ENETDOWN, libc::EACCES, libc::EPERM].contains(&errno)
        && [libc::SYS_connect, libc::SYS_sendto, libc::SYS_sendmsg].contains(&syscall)
    {
        return Some(FinalVerificationRuntimeViolation::NetworkAccessDenied);
    }
    if [libc::EACCES, libc::EPERM].contains(&errno)
        && [libc::SYS_open, libc::SYS_openat, 437, libc::SYS_execve, libc::SYS_execveat].contains(&syscall)
    {
        return Some(FinalVerificationRuntimeViolation::FilesystemAccessDenied);
    }
    None
}

/// Whether the two kernel mechanisms required by this launcher are available.
pub fn final_verification_backend_available() -> bool {
    crate::probe_landlock() && probe_network_namespace() && syscall_observation_available()
}

fn ensure_backend_available() -> Result<(), FinalVerificationError> {
    if !crate::probe_landlock() {
        return Err(FinalVerificationError::BackendUnavailable {
            reason: "Landlock is unavailable",
        });
    }
    if !probe_network_namespace() {
        return Err(FinalVerificationError::BackendUnavailable {
            reason: "network namespaces are unavailable",
        });
    }
    if !syscall_observation_available() {
        return Err(FinalVerificationError::BackendUnavailable {
            reason: "syscall observation is unavailable",
        });
    }
    Ok(())
}

#[cfg(target_arch = "x86_64")]
fn syscall_observation_available() -> bool {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return false;
    }
    if pid == 0 {
        let ok = begin_syscall_observation().is_ok();
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    }
    let mut status = 0;
    if unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) } != pid || !libc::WIFSTOPPED(status) {
        return false;
    }
    let detached = unsafe {
        libc::ptrace(
            libc::PTRACE_DETACH,
            pid,
            std::ptr::null_mut::<libc::c_void>(),
            std::ptr::null_mut::<libc::c_void>(),
        )
    } == 0;
    detached && unsafe { libc::waitpid(pid, &mut status, 0) } == pid && libc::WIFEXITED(status)
}

#[cfg(not(target_arch = "x86_64"))]
fn syscall_observation_available() -> bool {
    false
}

#[derive(Debug)]
struct PreparedRequest {
    argv: Vec<String>,
    worktree: PathBuf,
    working_directory: PathBuf,
    tool_runtime: Vec<PathBuf>,
    read_only_external_mounts: Vec<PathBuf>,
    output_directories: Vec<PathBuf>,
    environment: BTreeMap<String, String>,
}

impl PreparedRequest {
    fn new(request: FinalVerificationRequest) -> Result<Self, FinalVerificationViolation> {
        if request.argv.is_empty()
            || request
                .argv
                .iter()
                .any(|arg| arg.is_empty() || arg.contains('\0'))
        {
            return Err(FinalVerificationViolation::InvalidArgv);
        }
        let worktree = canonical_directory("worktree", &request.worktree)?;
        let working_directory =
            canonical_directory("working_directory", &request.working_directory)?;
        if !working_directory.starts_with(&worktree) {
            return Err(
                FinalVerificationViolation::WorkingDirectoryOutsideWorktree {
                    path: working_directory,
                },
            );
        }
        let tool_runtime = request
            .tool_runtime
            .iter()
            .map(|path| canonical_directory("tool_runtime", path))
            .collect::<Result<Vec<_>, _>>()?;
        let read_only_external_mounts = request
            .read_only_external_mounts
            .iter()
            .map(|path| canonical_directory("read_only_external_mount", path))
            .collect::<Result<Vec<_>, _>>()?;

        let mut output_directories = Vec::with_capacity(request.output_directories.len());
        for output in request.output_directories {
            if output.as_os_str().is_empty()
                || output.is_absolute()
                || output.components().any(|component| {
                    matches!(
                        component,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                })
            {
                return Err(FinalVerificationViolation::InvalidOutputDirectory { path: output });
            }
            let path = worktree.join(&output);
            if path.exists() {
                return Err(FinalVerificationViolation::OutputOnlyPreexisting { path });
            }
            output_directories.push(path);
        }
        for (index, first) in output_directories.iter().enumerate() {
            for second in &output_directories[index + 1..] {
                if first.starts_with(second) || second.starts_with(first) {
                    return Err(FinalVerificationViolation::OverlappingOutputDirectories {
                        first: first.clone(),
                        second: second.clone(),
                    });
                }
            }
        }

        Ok(Self {
            argv: request.argv,
            worktree,
            working_directory,
            tool_runtime,
            read_only_external_mounts,
            output_directories,
            environment: request.environment,
        })
    }

    fn create_empty_output_directories(&self) -> Result<(), FinalVerificationError> {
        for path in &self.output_directories {
            std::fs::create_dir(path).map_err(|source| {
                FinalVerificationError::OutputPreparation {
                    path: path.clone(),
                    source,
                }
            })?;
        }
        Ok(())
    }
}

fn canonical_directory(
    field: &'static str,
    path: &Path,
) -> Result<PathBuf, FinalVerificationViolation> {
    let canonical =
        path.canonicalize()
            .map_err(|_| FinalVerificationViolation::InvalidDirectory {
                field,
                path: path.to_path_buf(),
            })?;
    if !canonical.is_dir() {
        return Err(FinalVerificationViolation::InvalidDirectory {
            field,
            path: canonical,
        });
    }
    Ok(canonical)
}

fn apply_filesystem_policy(
    worktree: &Path,
    tool_runtime: &[PathBuf],
    external_mounts: &[PathBuf],
    output_directories: &[PathBuf],
) -> anyhow::Result<()> {
    let abi = ABI::V3;
    let full = AccessFs::from_all(abi);
    let read_exec = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;
    let mut ruleset = Ruleset::default()
        .handle_access(full)?
        .create()?
        .add_rule(PathBeneath::new(PathFd::new(worktree)?, read_exec))?;

    for path in tool_runtime.iter().chain(external_mounts) {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(path)?, read_exec))?;
    }
    for path in output_directories {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(path)?, full))?;
    }
    // The process needs these character devices for conventional tool startup,
    // but no other host paths are granted.
    for device in ["/dev/null", "/dev/zero", "/dev/urandom"] {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(device)?, full))?;
    }
    ruleset.restrict_self()?;
    Ok(())
}

fn probe_network_namespace() -> bool {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return false;
    }
    if pid == 0 {
        let ok = unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) } == 0;
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    }
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) >= 0 }
    &&libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
}

fn enter_isolated_network_namespace() -> io::Result<()> {
    let result = unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn request(worktree: &Path) -> FinalVerificationRequest {
        FinalVerificationRequest {
            argv: vec!["/bin/sh".into(), "-c".into(), "true".into()],
            worktree: worktree.to_path_buf(),
            working_directory: worktree.to_path_buf(),
            tool_runtime: ["/bin", "/usr", "/lib", "/lib64"]
                .into_iter()
                .filter(|path| Path::new(path).is_dir())
                .map(PathBuf::from)
                .collect(),
            read_only_external_mounts: Vec::new(),
            output_directories: vec![PathBuf::from("outputs")],
            environment: BTreeMap::from([("PATH".to_string(), "/bin:/usr/bin".to_string())]),
        }
    }

    #[test]
    fn preexisting_output_content_is_a_public_typed_sandbox_violation() {
        let worktree = TempDir::new().unwrap();
        std::fs::create_dir(worktree.path().join("outputs")).unwrap();
        std::fs::write(worktree.path().join("outputs/stale"), b"old output").unwrap();
        let mut req = request(worktree.path());
        req.argv[2] = "cat outputs/stale".into();
        let error = launch_final_verification(req).unwrap_err();
        assert!(matches!(
            error,
            FinalVerificationError::Violation(FinalVerificationViolation::OutputOnlyPreexisting {
                ..
            })
        ));
    }

    #[test]
    fn output_directory_must_be_relative_and_absent() {
        let worktree = TempDir::new().unwrap();
        let mut invalid = request(worktree.path());
        invalid.output_directories = vec![PathBuf::from("../escaped")];
        assert!(matches!(
            PreparedRequest::new(invalid),
            Err(FinalVerificationViolation::InvalidOutputDirectory { .. })
        ));
    }

    #[test]
    fn unsupported_backends_fail_closed() {
        if !final_verification_backend_available() {
            let worktree = TempDir::new().unwrap();
            assert!(matches!(
                launch_final_verification(request(worktree.path())),
                Err(FinalVerificationError::BackendUnavailable { .. })
            ));
        }
    }

    #[test]
    fn declared_external_and_new_output_work_when_backend_is_available() {
        if !final_verification_backend_available() {
            return;
        }
        let worktree = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        std::fs::write(external.path().join("input"), b"declared").unwrap();
        let mut req = request(worktree.path());
        req.read_only_external_mounts = vec![external.path().to_path_buf()];
        req.argv[2] = format!("cat {}/input > outputs/result", external.path().display());
        let result = launch_final_verification(req).unwrap();
        assert!(
            result.succeeded(),
            "stderr: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            std::fs::read(worktree.path().join("outputs/result")).unwrap(),
            b"declared"
        );
    }

    #[test]
    fn suppressed_undeclared_host_reads_are_typed_violations() {
        if !final_verification_backend_available() {
            return;
        }
        let worktree = TempDir::new().unwrap();
        let host_only = TempDir::new().unwrap();
        let secret = host_only.path().join("secret");
        std::fs::write(&secret, b"must not be readable").unwrap();
        let mut req = request(worktree.path());
        req.argv[2] = format!("cat {} >/dev/null 2>&1; exit 7", secret.display());
        assert!(matches!(
            launch_final_verification(req),
            Err(FinalVerificationError::RuntimeViolation {
                violation: FinalVerificationRuntimeViolation::FilesystemAccessDenied,
                ..
            })
        ));
    }

    #[test]
    fn suppressed_network_access_is_a_typed_violation() {
        if !final_verification_backend_available() || !Path::new("/bin/bash").is_file() {
            return;
        }
        let worktree = TempDir::new().unwrap();
        let mut req = request(worktree.path());
        req.argv = vec![
            "/bin/bash".into(),
            "-c".into(),
            "exec 3<>/dev/tcp/1.1.1.1/80 2>/dev/null".into(),
        ];
        assert!(
            matches!(
                launch_final_verification(req),
                Err(FinalVerificationError::RuntimeViolation {
                    violation: FinalVerificationRuntimeViolation::NetworkAccessDenied,
                    ..
                })
            ),
            "network namespace unexpectedly allowed a TCP connection"
        );
    }

    #[test]
    fn child_stderr_cannot_forge_a_runtime_violation() {
        if !final_verification_backend_available() {
            return;
        }
        let worktree = TempDir::new().unwrap();
        let mut req = request(worktree.path());
        req.argv[2] = "printf 'permission denied\\n' >&2; exit 9".into();
        let result = launch_final_verification(req).unwrap();
        assert_eq!(result.exit_code, Some(9));
        assert_eq!(result.stderr, b"permission denied\n");
    }
}

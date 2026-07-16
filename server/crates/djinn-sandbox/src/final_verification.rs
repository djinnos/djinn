//! Fail-closed launcher for reusable final verification on Linux.
//!
//! This module intentionally does not use [`crate::SANDBOX`] or the agent-shell
//! policy.  Those APIs may select a heuristic fallback and grant broad host
//! reads/writes, neither of which is acceptable evidence for reusable
//! verification.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::io;
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

/// Launcher failures are typed so callers can mark attempts ineligible without
/// treating a weaker execution as reusable evidence.
#[derive(Debug, thiserror::Error)]
pub enum FinalVerificationError {
    #[error("final-verification isolation backend is unavailable: {reason}")]
    BackendUnavailable { reason: &'static str },
    #[error("final-verification sandbox violation: {0}")]
    Violation(#[from] FinalVerificationViolation),
    #[error("failed to prepare output directory {path}: {source}")]
    OutputPreparation { path: PathBuf, source: io::Error },
    #[error("failed to launch isolated final verification: {0}")]
    Launch(#[source] io::Error),
}

/// Execute a final-verification command under the strict reusable policy.
///
/// Availability is checked before filesystem mutation.  There is deliberately
/// no fallback path: lack of Landlock or an isolated network namespace returns
/// [`FinalVerificationError::BackendUnavailable`]. Landlock and network
/// namespace denials are represented by the child's ordinary non-success
/// result. Linux does not expose an attributable Landlock-denial signal here,
/// so stderr text and generic permission errors are deliberately not classified
/// as typed sandbox violations.
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
                .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))
        });
    }

    let output = command.output().map_err(FinalVerificationError::Launch)?;
    Ok(FinalVerificationResult {
        exit_code: output.status.code(),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// Whether the two kernel mechanisms required by this launcher are available.
pub fn final_verification_backend_available() -> bool {
    crate::probe_landlock() && probe_network_namespace()
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
    Ok(())
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
        if request.argv.first().is_none_or(String::is_empty)
            || request.argv.iter().any(|arg| arg.contains('\0'))
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
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {
                    return Err(FinalVerificationViolation::OutputOnlyPreexisting { path });
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => {
                    return Err(FinalVerificationViolation::InvalidOutputDirectory { path });
                }
            }
            let parent = path
                .parent()
                .and_then(|parent| parent.canonicalize().ok())
                .filter(|parent| parent.starts_with(&worktree));
            if parent.is_none() {
                return Err(FinalVerificationViolation::InvalidOutputDirectory { path });
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
    (unsafe { libc::waitpid(pid, &mut status, 0) >= 0 })
        && libc::WIFEXITED(status)
        && libc::WEXITSTATUS(status) == 0
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
            FinalVerificationError::Violation(
                FinalVerificationViolation::OutputOnlyPreexisting { .. }
            )
        ));
        assert_eq!(
            std::fs::read(worktree.path().join("outputs/stale")).unwrap(),
            b"old output"
        );
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
    fn suppressed_undeclared_host_reads_are_non_success_results() {
        if !final_verification_backend_available() {
            return;
        }
        let worktree = TempDir::new().unwrap();
        let host_only = TempDir::new().unwrap();
        let secret = host_only.path().join("secret");
        std::fs::write(&secret, b"must not be readable").unwrap();
        let mut req = request(worktree.path());
        req.argv[2] = format!("cat {} >/dev/null 2>&1; exit 7", secret.display());
        let result = launch_final_verification(req).unwrap();
        assert!(!result.succeeded());
        assert!(result.stderr.is_empty());
    }

    #[test]
    fn suppressed_network_access_is_a_non_success_result() {
        if !final_verification_backend_available() || !Path::new("/bin/bash").is_file() {
            return;
        }
        let worktree = TempDir::new().unwrap();
        let mut req = request(worktree.path());
        req.argv = vec![
            "/bin/bash".into(),
            "-c".into(),
            "{ exec 3<>/dev/tcp/1.1.1.1/80; } 2>/dev/null".into(),
        ];
        let result = launch_final_verification(req).unwrap();
        assert!(
            !result.succeeded(),
            "network namespace allowed a TCP connection"
        );
        assert!(result.stderr.is_empty());
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

    #[test]
    fn ordinary_mode_permission_failure_is_an_ordinary_result() {
        use std::os::unix::fs::PermissionsExt;

        if !final_verification_backend_available() {
            return;
        }
        let worktree = TempDir::new().unwrap();
        let locked = worktree.path().join("locked");
        std::fs::write(&locked, b"allowed path, denied by Unix mode").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut req = request(worktree.path());
        req.argv[2] = "cat locked >/dev/null 2>&1; exit 7".into();
        let result = launch_final_verification(req).unwrap();
        assert_eq!(result.exit_code, Some(7));
        assert!(result.stderr.is_empty());
    }

    #[test]
    fn empty_non_executable_argument_is_passed_faithfully() {
        if !final_verification_backend_available() {
            return;
        }
        let worktree = TempDir::new().unwrap();
        let mut req = request(worktree.path());
        req.argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            "test \"$1\" = \"\"".into(),
            "--".into(),
            "".into(),
        ];
        let result = launch_final_verification(req).unwrap();
        assert!(result.succeeded());
    }
}

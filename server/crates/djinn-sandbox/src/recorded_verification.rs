//! Launcher for the `recorded` final-verification evidence tier.
//!
//! Deliberately the opposite of [`crate::final_verification`]: no user or
//! network namespace, no Landlock ruleset, no read-only worktree, and no
//! output-directory precondition. A recorded command runs as an ordinary
//! task-run Pod process, exactly like the compiles the agent itself performs,
//! so it sees — and reuses — the warm incremental build directory.
//!
//! That is the whole point. The attested tier's isolation is what forces a cold
//! full build per submission; the recorded tier trades the isolation guarantee
//! for the warm cache and says so in the plan's `evidence_tier`. What survives
//! is the part that actually answers "did anything change": the caller still
//! computes the whole-tree fingerprint and the environment identity before and
//! after this launcher runs, and rejects the result if either moved.
//!
//! What a recorded launch does NOT protect against, stated plainly:
//!
//! * a poisoned or stale incremental build cache producing a false pass;
//! * ambient state outside the declared environment influencing the command;
//! * the command writing anywhere the Pod user can write.
//!
//! Every task compile in a djinn Pod already runs under exactly those
//! conditions, so this is not a new exposure — but it is one the plan author
//! opts into by name rather than by omitting a flag.
//!
//! Two guards are retained even here, because both catch configuration
//! mistakes rather than adversarial behaviour: argv must be well-formed, and
//! the working directory must resolve inside the worktree.

#![cfg(target_os = "linux")]

use std::process::{Command, Stdio};
use std::time::Duration;

use crate::final_verification::{
    FinalVerificationError, FinalVerificationRequest, FinalVerificationResult,
    FinalVerificationViolation, wait_capturing_output,
};

/// Run one final-verification command warm, in the ambient Pod environment.
///
/// `tool_runtime`, `read_only_external_mounts`, and `output_directories` on the
/// request are ignored: they exist to describe a sandbox policy, and there is
/// no sandbox here. `output_directories` being ignored is what makes the
/// attested tier's "output dirs must be absent at launch" limitation — and its
/// "only the first command gets a writable directory" restriction — inapplicable
/// to this tier: every command can write the whole worktree, as an ordinary
/// build does.
pub fn launch_recorded_verification_with_timeout(
    request: FinalVerificationRequest,
    timeout: Duration,
) -> Result<FinalVerificationResult, FinalVerificationError> {
    if request.argv.first().is_none_or(String::is_empty)
        || request.argv.iter().any(|arg| arg.contains('\0'))
    {
        return Err(FinalVerificationViolation::InvalidArgv.into());
    }
    let worktree = canonical_directory("worktree", &request.worktree)?;
    let working_directory = canonical_directory("working_directory", &request.working_directory)?;
    // A configured `working_directory` that escapes the worktree is a plan
    // authoring error, and one that would run the command somewhere the
    // fingerprint does not describe. Fail closed on it in both tiers.
    if !working_directory.starts_with(&worktree) {
        return Err(
            FinalVerificationViolation::WorkingDirectoryOutsideWorktree {
                path: working_directory,
            }
            .into(),
        );
    }

    let mut command = Command::new(&request.argv[0]);
    command
        .args(&request.argv[1..])
        .current_dir(&working_directory)
        // Still `env_clear`: the recorded tier relaxes isolation, not the
        // requirement that every environment input be declared in the manifest.
        // Values arrive already resolved, split into identity-bearing and
        // volatile buckets by the caller.
        .env_clear()
        .envs(&request.environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = command.spawn().map_err(FinalVerificationError::Launch)?;
    wait_capturing_output(child, Some(timeout))
}

fn canonical_directory(
    field: &'static str,
    path: &std::path::Path,
) -> Result<std::path::PathBuf, FinalVerificationViolation> {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::*;

    fn request(worktree: &Path, script: &str) -> FinalVerificationRequest {
        FinalVerificationRequest {
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            worktree: worktree.to_path_buf(),
            working_directory: worktree.to_path_buf(),
            tool_runtime: Vec::new(),
            read_only_external_mounts: Vec::new(),
            output_directories: Vec::new(),
            environment: BTreeMap::from([("PATH".to_string(), "/bin:/usr/bin".to_string())]),
        }
    }

    /// The defining difference from the attested launcher: a pre-existing
    /// build directory is an input, not a violation, and it survives the run.
    #[test]
    fn preexisting_output_directory_is_reused_not_rejected() {
        let worktree = TempDir::new().unwrap();
        let warm = worktree.path().join("target");
        std::fs::create_dir(&warm).unwrap();
        std::fs::write(warm.join("artifact"), b"warm").unwrap();

        let result = launch_recorded_verification_with_timeout(
            request(
                worktree.path(),
                "cat target/artifact && echo more > target/second",
            ),
            Duration::from_secs(30),
        )
        .unwrap();

        assert!(
            result.succeeded(),
            "warm output must be readable: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stdout, b"warm");
        assert_eq!(std::fs::read(warm.join("artifact")).unwrap(), b"warm");
        assert!(
            warm.join("second").exists(),
            "the run must be able to write"
        );
    }

    /// Attested restricts writes to output directories, and only for the first
    /// command. A recorded run is an ordinary build: every command writes.
    #[test]
    fn every_command_can_write_the_worktree() {
        let worktree = TempDir::new().unwrap();
        for index in 0..3 {
            let result = launch_recorded_verification_with_timeout(
                request(worktree.path(), &format!("echo x > step{index}")),
                Duration::from_secs(30),
            )
            .unwrap();
            assert!(result.succeeded(), "command {index} could not write");
        }
        for index in 0..3 {
            assert!(worktree.path().join(format!("step{index}")).exists());
        }
    }

    /// `env_clear` still applies: an undeclared name is absent, so the recorded
    /// tier cannot silently depend on ambient environment it did not declare.
    #[test]
    fn undeclared_environment_is_not_inherited() {
        let worktree = TempDir::new().unwrap();
        let result = launch_recorded_verification_with_timeout(
            request(worktree.path(), "test -z \"$DJINN_RECORDED_PROBE\""),
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(
            result.succeeded(),
            "ambient environment leaked into the run"
        );
    }

    #[test]
    fn working_directory_outside_the_worktree_fails_closed() {
        let worktree = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let mut invalid = request(worktree.path(), "true");
        invalid.working_directory = outside.path().to_path_buf();
        assert!(matches!(
            launch_recorded_verification_with_timeout(invalid, Duration::from_secs(5)),
            Err(FinalVerificationError::Violation(
                FinalVerificationViolation::WorkingDirectoryOutsideWorktree { .. }
            ))
        ));
    }

    #[test]
    fn timeout_is_enforced_and_reported() {
        let worktree = TempDir::new().unwrap();
        let result = launch_recorded_verification_with_timeout(
            request(worktree.path(), "sleep 5"),
            Duration::from_millis(50),
        )
        .unwrap();
        assert!(result.timed_out);
        assert!(!result.succeeded());
    }

    #[test]
    fn malformed_argv_fails_closed() {
        let worktree = TempDir::new().unwrap();
        let mut invalid = request(worktree.path(), "true");
        invalid.argv = vec![String::new()];
        assert!(matches!(
            launch_recorded_verification_with_timeout(invalid, Duration::from_secs(5)),
            Err(FinalVerificationError::Violation(
                FinalVerificationViolation::InvalidArgv
            ))
        ));
        let mut nul = request(worktree.path(), "true");
        nul.argv = vec!["/bin/sh".into(), "-c".into(), "true\0".into()];
        assert!(matches!(
            launch_recorded_verification_with_timeout(nul, Duration::from_secs(5)),
            Err(FinalVerificationError::Violation(
                FinalVerificationViolation::InvalidArgv
            ))
        ));
        let _ = PathBuf::new();
    }
}

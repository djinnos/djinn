//! Persistence-agnostic final-verification execution boundary.
//!
//! This is deliberately the only composition point for identity, repository
//! fingerprinting, and the strict launcher.  Recording and reuse remain owned
//! by their respective coordinators.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use djinn_core::canonical_verify::{
    CanonicalCommandDescriptorV1, EnvironmentIdentityError, EnvironmentIdentityV1,
    ResolvedEnvironmentIdentityInputV1,
};
use djinn_core::clock::{Clock, SystemClock};
use djinn_git::{
    ResolvedExternalInputV1, VerificationInputDigestV1, VerificationInputFingerprint,
    VerificationInputFingerprintConfig, compute_verification_input_fingerprint_with_config,
};

use crate::final_verification::{
    FinalVerificationError, FinalVerificationRequest, launch_final_verification_with_timeout,
};

/// Reacquires resolved identity material at each consistency boundary.
pub type EnvironmentIdentityResolver = Arc<
    dyn Fn() -> Result<ResolvedEnvironmentIdentityInputV1, EnvironmentIdentityError> + Send + Sync,
>;

/// All resolved host material required to execute a canonical plan.
///
/// `resolve_environment_identity` must re-probe host material on every call.
/// The remaining fields are checked against its canonical manifest before F0.
#[derive(Clone)]
pub struct FinalVerificationExecutionRequest {
    pub worktree: PathBuf,
    pub resolve_environment_identity: EnvironmentIdentityResolver,
    pub fingerprint_config: VerificationInputFingerprintConfig,
    pub tool_runtime: Vec<PathBuf>,
    pub read_only_external_mounts: Vec<PathBuf>,
    /// Concrete output-only directories resolved from the manifest globs.
    pub output_directories: Vec<PathBuf>,
}

impl std::fmt::Debug for FinalVerificationExecutionRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FinalVerificationExecutionRequest")
            .field("worktree", &self.worktree)
            .field("fingerprint_config", &self.fingerprint_config)
            .field("tool_runtime", &self.tool_runtime)
            .field("read_only_external_mounts", &self.read_only_external_mounts)
            .field("output_directories", &self.output_directories)
            .finish_non_exhaustive()
    }
}

/// Ordered evidence for one configured command descriptor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalVerificationCommandEvidence {
    pub descriptor: CanonicalCommandDescriptorV1,
    pub started_at_unix_millis: u128,
    pub completed_at_unix_millis: u128,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

/// Stable reasons that make a run unsuitable for durable reusable evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinalVerificationIneligibilityReason {
    NonHermeticPlan,
    ManifestBindingMismatch {
        detail: String,
    },
    EnvironmentIdentityUnavailable {
        detail: String,
    },
    FingerprintUnavailable {
        detail: String,
    },
    FingerprintFailure {
        detail: String,
    },
    UndeclaredCommandEnvironment {
        name: String,
    },
    LauncherUnavailable {
        detail: String,
    },
    SandboxViolation {
        detail: String,
    },
    LaunchFailure {
        detail: String,
    },
    CommandTimedOut {
        check_id: String,
    },
    CommandFailed {
        check_id: String,
        exit_code: Option<i32>,
    },
    RequiredChecksNotCovered {
        missing: Vec<String>,
    },
    FingerprintChanged,
    EnvironmentChanged,
}

/// Complete execution material. `eligibility_reason` is `None` only for a
/// reusable success; ineligible attempts retain the partial evidence needed by
/// the authoritative recording coordinator without performing persistence.
#[derive(Clone, Debug)]
pub struct FinalVerificationExecutionEvidence {
    pub manifest_version: u32,
    pub pre_environment_identity: Option<EnvironmentIdentityV1>,
    pub post_environment_identity: Option<EnvironmentIdentityV1>,
    pub fingerprint_f0: Option<VerificationInputDigestV1>,
    pub fingerprint_f1: Option<VerificationInputDigestV1>,
    pub commands: Vec<FinalVerificationCommandEvidence>,
    pub eligibility_reason: Option<FinalVerificationIneligibilityReason>,
}

impl FinalVerificationExecutionEvidence {
    pub fn eligible(&self) -> bool {
        self.eligibility_reason.is_none()
    }
}

/// Resolve identity, compute F0, launch every descriptor in configured order,
/// and compute F1 plus identity at the consistency boundary. This function
/// neither reads nor writes `verify_runs`.
pub async fn execute_final_verification(
    request: FinalVerificationExecutionRequest,
) -> FinalVerificationExecutionEvidence {
    execute_final_verification_with_launcher(request, |request, timeout| {
        launch_final_verification_with_timeout(request, timeout)
    })
    .await
}

async fn execute_final_verification_with_launcher(
    request: FinalVerificationExecutionRequest,
    launch: impl Fn(
        FinalVerificationRequest,
        Duration,
    ) -> Result<
        crate::final_verification::FinalVerificationResult,
        FinalVerificationError,
    >,
) -> FinalVerificationExecutionEvidence {
    let initial_input = match (request.resolve_environment_identity)() {
        Ok(input) => input,
        Err(error) => {
            return FinalVerificationExecutionEvidence {
                manifest_version: 0,
                pre_environment_identity: None,
                post_environment_identity: None,
                fingerprint_f0: None,
                fingerprint_f1: None,
                commands: Vec::new(),
                eligibility_reason: Some(identity_reason(error)),
            };
        }
    };
    let manifest_version = initial_input.input_manifest.version;
    let mut evidence = FinalVerificationExecutionEvidence {
        manifest_version,
        pre_environment_identity: None,
        post_environment_identity: None,
        fingerprint_f0: None,
        fingerprint_f1: None,
        commands: Vec::new(),
        eligibility_reason: None,
    };

    let plan = initial_input.plan.clone();
    if !plan.hermeticity.hermetic || !plan.hermeticity.reusable || plan.hermeticity.network_access {
        return ineligible(
            evidence,
            FinalVerificationIneligibilityReason::NonHermeticPlan,
        );
    }
    let pre_identity = match EnvironmentIdentityV1::derive(initial_input.clone()) {
        Ok(identity) => identity,
        Err(error) => return ineligible(evidence, identity_reason(error)),
    };
    evidence.pre_environment_identity = Some(pre_identity.clone());

    let (external_mounts, output_directories) = match bind_manifest(&request, &initial_input) {
        Ok(binding) => binding,
        Err(reason) => return ineligible(evidence, reason),
    };

    let f0 = match fingerprint(&request.worktree, &request.fingerprint_config).await {
        Ok(digest) => digest,
        Err(reason) => return ineligible(evidence, reason),
    };
    evidence.fingerprint_f0 = Some(f0);

    let declared_environment: BTreeSet<_> = initial_input
        .input_manifest
        .environment_names
        .iter()
        .cloned()
        .collect();
    for (position, descriptor) in plan.commands.iter().cloned().enumerate() {
        let environment = match command_environment(
            &descriptor,
            &declared_environment,
            &initial_input.allowlisted_environment,
        ) {
            Ok(environment) => environment,
            Err(reason) => return ineligible(evidence, reason),
        };
        let started_at_unix_millis = now_millis();
        // Outputs are created exactly once. Subsequent descriptors retain the
        // strict read-only worktree and cannot obtain a broader host grant.
        let command_outputs = if position == 0 {
            output_directories.clone()
        } else {
            Vec::new()
        };
        let launched = launch(
            FinalVerificationRequest {
                argv: std::iter::once(descriptor.executable.clone())
                    .chain(descriptor.argv.iter().cloned())
                    .collect(),
                worktree: request.worktree.clone(),
                working_directory: request.worktree.join(&descriptor.working_directory),
                tool_runtime: request.tool_runtime.clone(),
                read_only_external_mounts: external_mounts.clone(),
                output_directories: command_outputs,
                environment,
            },
            Duration::from_secs(descriptor.timeout_seconds),
        );
        let completed_at_unix_millis = now_millis();
        let result = match launched {
            Ok(result) => result,
            Err(error) => return ineligible(evidence, launcher_reason(error)),
        };
        let command = FinalVerificationCommandEvidence {
            descriptor: descriptor.clone(),
            started_at_unix_millis,
            completed_at_unix_millis,
            exit_code: result.exit_code,
            timed_out: result.timed_out,
        };
        let reason = if result.timed_out {
            Some(FinalVerificationIneligibilityReason::CommandTimedOut {
                check_id: descriptor.check_id,
            })
        } else if !result.succeeded() {
            Some(FinalVerificationIneligibilityReason::CommandFailed {
                check_id: descriptor.check_id,
                exit_code: result.exit_code,
            })
        } else {
            None
        };
        evidence.commands.push(command);
        if let Some(reason) = reason {
            return ineligible(evidence, reason);
        }
    }

    let executed: BTreeSet<_> = evidence
        .commands
        .iter()
        .map(|command| command.descriptor.check_id.clone())
        .collect();
    let missing: Vec<_> = plan
        .required_checks
        .iter()
        .filter(|check| !executed.contains(*check))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return ineligible(
            evidence,
            FinalVerificationIneligibilityReason::RequiredChecksNotCovered { missing },
        );
    }

    let f1 = match fingerprint(&request.worktree, &request.fingerprint_config).await {
        Ok(digest) => digest,
        Err(reason) => return ineligible(evidence, reason),
    };
    evidence.fingerprint_f1 = Some(f1.clone());
    let post_input = match (request.resolve_environment_identity)() {
        Ok(input) => input,
        Err(error) => return ineligible(evidence, identity_reason(error)),
    };
    let post_identity = match EnvironmentIdentityV1::derive(post_input) {
        Ok(identity) => identity,
        Err(error) => return ineligible(evidence, identity_reason(error)),
    };
    evidence.post_environment_identity = Some(post_identity.clone());
    if evidence.fingerprint_f0.as_ref() != Some(&f1) {
        return ineligible(
            evidence,
            FinalVerificationIneligibilityReason::FingerprintChanged,
        );
    }
    if pre_identity != post_identity {
        return ineligible(
            evidence,
            FinalVerificationIneligibilityReason::EnvironmentChanged,
        );
    }
    evidence
}

async fn fingerprint(
    worktree: &PathBuf,
    config: &VerificationInputFingerprintConfig,
) -> Result<VerificationInputDigestV1, FinalVerificationIneligibilityReason> {
    match compute_verification_input_fingerprint_with_config(worktree, config).await {
        Ok(VerificationInputFingerprint::Available(digest)) => Ok(digest),
        Ok(VerificationInputFingerprint::Unavailable(reason)) => Err(
            FinalVerificationIneligibilityReason::FingerprintUnavailable {
                detail: reason.to_string(),
            },
        ),
        Err(error) => Err(FinalVerificationIneligibilityReason::FingerprintFailure {
            detail: error.to_string(),
        }),
    }
}

/// Bind every filesystem-facing execution value to the identity manifest before
/// it can affect either F0 or the launcher policy.
fn bind_manifest(
    request: &FinalVerificationExecutionRequest,
    input: &ResolvedEnvironmentIdentityInputV1,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), FinalVerificationIneligibilityReason> {
    if request.fingerprint_config.manifest != input.input_manifest {
        return Err(manifest_binding(
            "fingerprint manifest differs from identity manifest",
        ));
    }
    let declared: BTreeSet<_> = input
        .input_manifest
        .read_only_external_inputs
        .iter()
        .map(|entry| entry.id.as_str())
        .collect();
    let configured: BTreeSet<_> = request
        .fingerprint_config
        .external_inputs
        .iter()
        .map(|entry| entry.id.as_str())
        .collect();
    if configured.len() != request.fingerprint_config.external_inputs.len()
        || configured != declared
    {
        return Err(manifest_binding(
            "resolved external mounts differ from manifest declarations",
        ));
    }
    let external_mounts = resolved_mount_paths(&request.fingerprint_config.external_inputs);
    if request.read_only_external_mounts != external_mounts {
        return Err(manifest_binding(
            "launcher external mounts differ from fingerprint resolved mounts",
        ));
    }
    let output_directories = output_directories(&input.input_manifest.output_only_globs)?;
    if request.output_directories != output_directories {
        return Err(manifest_binding(
            "launcher output directories differ from manifest output-only globs",
        ));
    }
    Ok((external_mounts, output_directories))
}

fn resolved_mount_paths(inputs: &[ResolvedExternalInputV1]) -> Vec<PathBuf> {
    inputs.iter().map(|input| input.path.clone()).collect()
}

/// Convert an output glob into the literal directory prefix the strict launcher
/// can make writable. Globs without a literal prefix would require a broader
/// write grant and therefore fail closed.
fn output_directories(
    globs: &[String],
) -> Result<Vec<PathBuf>, FinalVerificationIneligibilityReason> {
    let mut directories = BTreeSet::new();
    for glob in globs {
        let mut prefix = PathBuf::new();
        for component in Path::new(glob).components() {
            let Component::Normal(component) = component else {
                return Err(manifest_binding(
                    "output-only glob is not a safe relative path",
                ));
            };
            let component = component.to_string_lossy();
            if component.contains(['*', '?', '[', '{']) {
                break;
            }
            prefix.push(component.as_ref());
        }
        if prefix.as_os_str().is_empty() {
            return Err(manifest_binding(
                "output-only glob has no directory prefix for strict launcher",
            ));
        }
        directories.insert(prefix);
    }
    Ok(directories.into_iter().collect())
}

fn manifest_binding(detail: impl Into<String>) -> FinalVerificationIneligibilityReason {
    FinalVerificationIneligibilityReason::ManifestBindingMismatch {
        detail: detail.into(),
    }
}

fn command_environment(
    descriptor: &CanonicalCommandDescriptorV1,
    manifest_names: &BTreeSet<String>,
    allowlisted: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, FinalVerificationIneligibilityReason> {
    descriptor
        .environment_names
        .iter()
        .map(|name| {
            if !manifest_names.contains(name) || !allowlisted.contains_key(name) {
                return Err(
                    FinalVerificationIneligibilityReason::UndeclaredCommandEnvironment {
                        name: name.clone(),
                    },
                );
            }
            Ok((name.clone(), allowlisted[name].clone()))
        })
        .collect()
}

fn launcher_reason(error: FinalVerificationError) -> FinalVerificationIneligibilityReason {
    match error {
        FinalVerificationError::BackendUnavailable { reason } => {
            FinalVerificationIneligibilityReason::LauncherUnavailable {
                detail: reason.to_owned(),
            }
        }
        FinalVerificationError::Violation(violation) => {
            FinalVerificationIneligibilityReason::SandboxViolation {
                detail: violation.to_string(),
            }
        }
        FinalVerificationError::OutputPreparation { .. } | FinalVerificationError::Launch(_) => {
            FinalVerificationIneligibilityReason::LaunchFailure {
                detail: error.to_string(),
            }
        }
    }
}

fn identity_reason(error: EnvironmentIdentityError) -> FinalVerificationIneligibilityReason {
    FinalVerificationIneligibilityReason::EnvironmentIdentityUnavailable {
        detail: error.to_string(),
    }
}

fn ineligible(
    mut evidence: FinalVerificationExecutionEvidence,
    reason: FinalVerificationIneligibilityReason,
) -> FinalVerificationExecutionEvidence {
    evidence.eligibility_reason = Some(reason);
    evidence
}

fn now_millis() -> u128 {
    SystemClock::new()
        .now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command as ProcessCommand;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use djinn_core::canonical_verify::{
        CanonicalFinalVerificationPlanV1, CanonicalHermeticityV1, ImmutableImageV1,
        LockfileDigestV1, ToolProbeStatus, ToolProbeV1, VerificationInputManifestV1,
    };
    use tempfile::TempDir;

    use crate::final_verification::{FinalVerificationResult, FinalVerificationViolation};

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn descriptor(check_id: &str) -> CanonicalCommandDescriptorV1 {
        CanonicalCommandDescriptorV1 {
            check_id: check_id.into(),
            executable: "verify-tool".into(),
            argv: vec![check_id.into()],
            working_directory: ".".into(),
            environment_names: Vec::new(),
            timeout_seconds: 60,
            descriptor_revision: 1,
        }
    }

    fn identity_input(
        commands: Vec<CanonicalCommandDescriptorV1>,
        output_only_globs: Vec<String>,
    ) -> ResolvedEnvironmentIdentityInputV1 {
        let required_checks = commands.iter().map(|command| command.check_id.clone()).collect();
        ResolvedEnvironmentIdentityInputV1 {
            schema_version: 1,
            canonicalization_version: 1,
            plan: CanonicalFinalVerificationPlanV1 {
                version: 1,
                profile_id: "test".into(),
                profile_revision: 1,
                commands,
                required_checks,
                hermeticity: CanonicalHermeticityV1 {
                    hermetic: true,
                    reusable: true,
                    network_access: false,
                },
            },
            input_manifest: VerificationInputManifestV1 {
                version: 1,
                repo_paths: Vec::new(),
                environment_names: Vec::new(),
                read_only_external_inputs: Vec::new(),
                output_only_globs,
            },
            image: ImmutableImageV1 {
                reference: "test-image".into(),
                digest: DIGEST.into(),
            },
            tool_probes: vec![ToolProbeV1 {
                tool: "verify-tool".into(),
                version: "1".into(),
                executable_digest: DIGEST.into(),
                status: ToolProbeStatus::Passed,
            }],
            runner_version: "runner-1".into(),
            lockfile_digests: vec![LockfileDigestV1 {
                path: "lock".into(),
                digest: DIGEST.into(),
            }],
            target: "test-target".into(),
            features: Vec::new(),
            allowlisted_environment: BTreeMap::new(),
        }
    }

    fn git_worktree() -> TempDir {
        let directory = tempfile::tempdir().expect("create worktree");
        fs::write(directory.path().join("input.txt"), "before").expect("write input");
        for args in [
            ["init", "-b", "main"].as_slice(),
            ["config", "user.email", "test@example.com"].as_slice(),
            ["config", "user.name", "Test User"].as_slice(),
            ["add", "input.txt"].as_slice(),
            ["commit", "-m", "initial"].as_slice(),
        ] {
            let status = ProcessCommand::new("git")
                .args(args)
                .current_dir(directory.path())
                .status()
                .expect("run git");
            assert!(status.success(), "git command failed: {args:?}");
        }
        directory
    }

    fn request(
        worktree: &Path,
        input: ResolvedEnvironmentIdentityInputV1,
        resolver: EnvironmentIdentityResolver,
    ) -> FinalVerificationExecutionRequest {
        let output_directories = output_directories(&input.input_manifest.output_only_globs)
            .expect("valid output-only directories");
        FinalVerificationExecutionRequest {
            worktree: worktree.to_path_buf(),
            resolve_environment_identity: resolver,
            fingerprint_config: VerificationInputFingerprintConfig {
                base_ref: "main".into(),
                manifest: input.input_manifest,
                external_inputs: Vec::new(),
            },
            tool_runtime: Vec::new(),
            read_only_external_mounts: Vec::new(),
            output_directories,
        }
    }

    fn static_resolver(input: ResolvedEnvironmentIdentityInputV1) -> EnvironmentIdentityResolver {
        Arc::new(move || Ok(input.clone()))
    }

    fn succeeded() -> FinalVerificationResult {
        FinalVerificationResult {
            exit_code: Some(0),
            timed_out: false,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn output_only_glob_requires_a_literal_launcher_directory() {
        assert_eq!(
            output_directories(&["out/**".into()]).expect("directory"),
            vec![PathBuf::from("out")]
        );
        assert!(matches!(
            output_directories(&["**/*.log".into()]),
            Err(FinalVerificationIneligibilityReason::ManifestBindingMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn repository_mutation_during_command_makes_fingerprint_ineligible() {
        let worktree = git_worktree();
        let input = identity_input(vec![descriptor("check")], Vec::new());
        let request = request(worktree.path(), input.clone(), static_resolver(input));
        let changed_path = worktree.path().join("input.txt");

        let evidence = execute_final_verification_with_launcher(request, move |_, _| {
            fs::write(&changed_path, "after").expect("mutate repository input");
            Ok(succeeded())
        })
        .await;

        assert_eq!(
            evidence.eligibility_reason,
            Some(FinalVerificationIneligibilityReason::FingerprintChanged)
        );
        assert_ne!(evidence.fingerprint_f0, evidence.fingerprint_f1);
    }

    #[tokio::test]
    async fn refreshed_probe_material_change_makes_environment_ineligible() {
        let worktree = git_worktree();
        let input = identity_input(vec![descriptor("check")], Vec::new());
        let changed = {
            let mut changed = input.clone();
            changed.tool_probes[0].executable_digest =
                "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into();
            changed
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver_calls = Arc::clone(&calls);
        let initial = input.clone();
        let resolver: EnvironmentIdentityResolver = Arc::new(move || {
            if resolver_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(input.clone())
            } else {
                Ok(changed.clone())
            }
        });
        let request = request(worktree.path(), initial, resolver);

        let evidence = execute_final_verification_with_launcher(request, |_, _| Ok(succeeded())).await;

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            evidence.eligibility_reason,
            Some(FinalVerificationIneligibilityReason::EnvironmentChanged)
        );
        assert_eq!(evidence.fingerprint_f0, evidence.fingerprint_f1);
    }

    #[tokio::test]
    async fn preexisting_manifest_output_is_rejected_before_command_evidence() {
        let worktree = git_worktree();
        fs::create_dir(worktree.path().join("out")).expect("create stale output directory");
        fs::write(worktree.path().join("out/stale"), "stale").expect("write stale output");
        let input = identity_input(vec![descriptor("check")], vec!["out/**".into()]);
        let request = request(worktree.path(), input.clone(), static_resolver(input));
        let launches = Arc::new(AtomicUsize::new(0));
        let launcher_calls = Arc::clone(&launches);

        let evidence = execute_final_verification_with_launcher(request, move |launch, _| {
            launcher_calls.fetch_add(1, Ordering::SeqCst);
            let path = launch.worktree.join(&launch.output_directories[0]);
            assert!(path.exists(), "strict launcher observes the pre-existing output");
            Err(FinalVerificationError::Violation(
                FinalVerificationViolation::OutputOnlyPreexisting { path },
            ))
        })
        .await;

        assert_eq!(launches.load(Ordering::SeqCst), 1);
        assert!(evidence.commands.is_empty(), "child never produced command evidence");
        assert!(matches!(
            evidence.eligibility_reason,
            Some(FinalVerificationIneligibilityReason::SandboxViolation { .. })
        ));
    }

    #[tokio::test]
    async fn commands_launch_in_order_and_each_must_pass() {
        let worktree = git_worktree();
        let input = identity_input(
            vec![descriptor("first"), descriptor("second")],
            Vec::new(),
        );
        let request = request(worktree.path(), input.clone(), static_resolver(input));
        let launched = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&launched);

        let evidence = execute_final_verification_with_launcher(request, move |launch, _| {
            let check = launch.argv[1].clone();
            observed.lock().expect("lock observed commands").push(check.clone());
            Ok(FinalVerificationResult {
                exit_code: if check == "second" { Some(1) } else { Some(0) },
                timed_out: false,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        })
        .await;

        assert_eq!(
            *launched.lock().expect("lock observed commands"),
            vec!["first", "second"]
        );
        assert_eq!(evidence.commands.len(), 2);
        assert_eq!(
            evidence.eligibility_reason,
            Some(FinalVerificationIneligibilityReason::CommandFailed {
                check_id: "second".into(),
                exit_code: Some(1),
            })
        );
    }
}

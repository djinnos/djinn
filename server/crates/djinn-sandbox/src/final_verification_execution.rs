//! Persistence-agnostic final-verification execution boundary.
//!
//! This is deliberately the only composition point for identity, repository
//! fingerprinting, and the strict launcher.  Recording and reuse remain owned
//! by their respective coordinators.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

use djinn_core::canonical_verify::{
    CanonicalCommandDescriptorV1, EnvironmentIdentityError, EnvironmentIdentityV1,
    ResolvedEnvironmentIdentityInputV1,
};
use djinn_core::clock::{Clock, SystemClock};
use djinn_git::{
    VerificationInputDigestV1, VerificationInputFingerprint, VerificationInputFingerprintConfig,
    compute_verification_input_fingerprint_with_config,
};

use crate::final_verification::{
    FinalVerificationError, FinalVerificationRequest, launch_final_verification_with_timeout,
};

/// All resolved host material required to execute a canonical plan.  Tool
/// probes, image identity, lockfiles, target, features, and allowlisted
/// environment are carried by `environment_identity_input` and are validated
/// by `EnvironmentIdentityV1::derive` before any command may run.
#[derive(Clone, Debug)]
pub struct FinalVerificationExecutionRequest {
    pub worktree: PathBuf,
    pub environment_identity_input: ResolvedEnvironmentIdentityInputV1,
    pub fingerprint_config: VerificationInputFingerprintConfig,
    pub tool_runtime: Vec<PathBuf>,
    pub read_only_external_mounts: Vec<PathBuf>,
    /// Concrete output-only directories resolved from the manifest globs.
    pub output_directories: Vec<PathBuf>,
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
    let manifest_version = request.environment_identity_input.input_manifest.version;
    let mut evidence = FinalVerificationExecutionEvidence {
        manifest_version,
        pre_environment_identity: None,
        post_environment_identity: None,
        fingerprint_f0: None,
        fingerprint_f1: None,
        commands: Vec::new(),
        eligibility_reason: None,
    };

    let plan = request.environment_identity_input.plan.clone();
    if !plan.hermeticity.hermetic || !plan.hermeticity.reusable || plan.hermeticity.network_access {
        return ineligible(
            evidence,
            FinalVerificationIneligibilityReason::NonHermeticPlan,
        );
    }
    let pre_identity =
        match EnvironmentIdentityV1::derive(request.environment_identity_input.clone()) {
            Ok(identity) => identity,
            Err(error) => return ineligible(evidence, identity_reason(error)),
        };
    evidence.pre_environment_identity = Some(pre_identity.clone());

    let f0 = match fingerprint(&request.worktree, &request.fingerprint_config).await {
        Ok(digest) => digest,
        Err(reason) => return ineligible(evidence, reason),
    };
    evidence.fingerprint_f0 = Some(f0);

    let declared_environment: BTreeSet<_> = request
        .environment_identity_input
        .input_manifest
        .environment_names
        .iter()
        .cloned()
        .collect();
    for (position, descriptor) in plan.commands.iter().cloned().enumerate() {
        let environment = match command_environment(
            &descriptor,
            &declared_environment,
            &request.environment_identity_input.allowlisted_environment,
        ) {
            Ok(environment) => environment,
            Err(reason) => return ineligible(evidence, reason),
        };
        let started_at_unix_millis = now_millis();
        // Outputs are created exactly once. Subsequent descriptors retain the
        // strict read-only worktree and cannot obtain a broader host grant.
        let output_directories = if position == 0 {
            request.output_directories.clone()
        } else {
            Vec::new()
        };
        let launched = launch_final_verification_with_timeout(
            FinalVerificationRequest {
                argv: std::iter::once(descriptor.executable.clone())
                    .chain(descriptor.argv.iter().cloned())
                    .collect(),
                worktree: request.worktree.clone(),
                working_directory: request.worktree.join(&descriptor.working_directory),
                tool_runtime: request.tool_runtime.clone(),
                read_only_external_mounts: request.read_only_external_mounts.clone(),
                output_directories,
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
    let post_identity = match EnvironmentIdentityV1::derive(request.environment_identity_input) {
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

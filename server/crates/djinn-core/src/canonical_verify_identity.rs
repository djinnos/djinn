//! Versioned, pure final-verification identity contracts.
//!
//! These types deliberately contain resolved values only. Resolving stack
//! configuration, inspecting the worktree, and executing tool probes belong to
//! the executor boundary, not to canonical identity derivation.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const FINAL_VERIFICATION_PLAN_VERSION_V1: u32 = 1;
pub const VERIFICATION_INPUT_MANIFEST_VERSION_V1: u32 = 1;
pub const ENVIRONMENT_IDENTITY_SCHEMA_VERSION_V1: u32 = 1;
pub const ENVIRONMENT_IDENTITY_CANONICALIZATION_VERSION_V1: u32 = 1;
pub const ENVIRONMENT_IDENTITY_SCHEMA_VERSION_V2: u32 = 2;
pub const ENVIRONMENT_IDENTITY_CANONICALIZATION_VERSION_V2: u32 = 2;

/// A command is an ordered part of a final-verification plan. Its position is
/// significant and is therefore never sorted during canonicalization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalCommandDescriptorV1 {
    pub check_id: String,
    pub executable: String,
    pub argv: Vec<String>,
    pub working_directory: String,
    pub environment_names: Vec<String>,
    pub timeout_seconds: u64,
    pub descriptor_revision: u32,
}

/// Resolved execution-isolation declaration from the final-verification plan.
/// Its values are identity material: different network or reuse guarantees
/// cannot safely share a verification result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalHermeticityV1 {
    pub hermetic: bool,
    pub reusable: bool,
    pub network_access: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalFinalVerificationPlanV1 {
    pub version: u32,
    pub profile_id: String,
    pub profile_revision: u32,
    /// Ordered exactly as configured; changing command order changes identity.
    pub commands: Vec<CanonicalCommandDescriptorV1>,
    /// Set-like: normalized lexicographically.
    pub required_checks: Vec<String>,
    /// Resolved isolation and reuse guarantees for this plan.
    pub hermeticity: CanonicalHermeticityV1,
}

/// One configured selection rule after configuration parsing has completed.
/// Rules remain in configuration order, while their match globs and group
/// references are set-like declarations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedVerificationSelectionRuleV1 {
    pub matches: Vec<String>,
    pub command_groups: Vec<String>,
}

/// Resolved selection material independent of the complete-tree input
/// fingerprint. It is identity material because results from a different
/// selection cannot safely be reused, even if flattened commands coincide.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolvedVerificationSelectionV1 {
    /// Compatibility representation for the original flat `commands` plan.
    LegacyFlatPlan,
    SelectedGroups {
        /// Ordered exactly as configured.
        selection_rules: Vec<ResolvedVerificationSelectionRuleV1>,
        /// Set-like selected group identities, normalized lexicographically.
        selected_command_groups: Vec<String>,
    },
}

impl ResolvedVerificationSelectionV1 {
    pub fn legacy_flat_plan() -> Self {
        Self::LegacyFlatPlan
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredExternalInputV1 {
    pub id: String,
    pub locator: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationInputManifestV1 {
    pub version: u32,
    /// Set-like declarations of paths included by the resolved input producer.
    pub repo_paths: Vec<String>,
    /// Set-like declaration of environment names permitted as inputs.
    pub environment_names: Vec<String>,
    pub read_only_external_inputs: Vec<DeclaredExternalInputV1>,
    /// Set-like; output-only paths must not ambiguously overlap inputs.
    pub output_only_globs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolProbeStatus {
    Passed,
    Failed,
}

/// An already-observed tool probe. Identity creation validates that each
/// declaration was probed successfully; it never executes the probe itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolProbeV1 {
    pub tool: String,
    pub version: String,
    pub executable_digest: String,
    pub status: ToolProbeStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImmutableImageV1 {
    /// Image reference, optionally retaining the human-readable source tag.
    pub reference: String,
    /// Immutable `sha256:<64 lowercase hex>` content address.
    pub digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockfileDigestV1 {
    pub path: String,
    pub digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedCatalogServiceIdentityV1 {
    pub preset_id: String,
    pub service_type: String,
    pub image_reference: String,
    pub image_digest: String,
    pub port: i32,
    pub exported_environment_names: Vec<String>,
    pub verification_protocol_revision: u32,
    pub effective_configuration_digest: String,
}

/// Fully resolved material for identity derivation. All maps are BTreeMaps so
/// callers cannot accidentally provide non-deterministic iteration order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedEnvironmentIdentityInputV1 {
    pub schema_version: u32,
    pub canonicalization_version: u32,
    pub plan: CanonicalFinalVerificationPlanV1,
    /// Resolved path-rule material and selected named groups for this run.
    pub selection: ResolvedVerificationSelectionV1,
    pub input_manifest: VerificationInputManifestV1,
    pub image: ImmutableImageV1,
    pub tool_probes: Vec<ToolProbeV1>,
    pub runner_version: String,
    pub lockfile_digests: Vec<LockfileDigestV1>,
    pub target: String,
    pub features: Vec<String>,
    pub allowlisted_environment: BTreeMap<String, String>,
    #[serde(default)]
    pub services: Vec<ResolvedCatalogServiceIdentityV1>,
}

/// Persistable canonical identity. JSON is canonical bytes decoded as UTF-8,
/// and `digest` is the lowercase SHA-256 of exactly those bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentIdentityV1 {
    pub schema_version: u32,
    pub canonicalization_version: u32,
    pub canonical_json: String,
    pub digest: String,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum EnvironmentIdentityError {
    #[error("unsupported {kind} version {version}")]
    UnsupportedVersion { kind: &'static str, version: u32 },
    #[error("missing resolved {field}")]
    MissingResolved { field: &'static str },
    #[error("duplicate {kind}: {value}")]
    Duplicate { kind: &'static str, value: String },
    #[error("required check {check_id} has no command descriptor")]
    UncoveredRequiredCheck { check_id: String },
    #[error("missing resolved tool probe for executable {executable}")]
    MissingToolProbe { executable: String },
    #[error("tool probe {tool} did not pass")]
    FailedToolProbe { tool: String },
    #[error("invalid {kind} digest: {value}")]
    MalformedDigest { kind: &'static str, value: String },
    #[error("ambiguous declaration: {detail}")]
    AmbiguousDeclaration { detail: String },
    #[error("reusable final verification must be hermetic and deny network access")]
    InvalidReusableHermeticity,
    #[error("selected command group {group} is not referenced by a selection rule")]
    UnknownSelectedCommandGroup { group: String },
}

impl EnvironmentIdentityV1 {
    /// Validate, normalize all set-like fields, serialize stable JSON, and hash
    /// it. No filesystem access or command execution occurs here.
    pub fn derive(
        input: ResolvedEnvironmentIdentityInputV1,
    ) -> Result<Self, EnvironmentIdentityError> {
        let normalized = input.canonicalized()?;
        // V1 rows predate catalog services. Serialize the legacy shape exactly
        // (without an empty `services` member) so persisted V1 digests retain
        // their meaning; V2 is the additive service-aware contract.
        let canonical_json = if normalized.schema_version == 1 {
            let mut value = serde_json::to_value(&normalized)
                .expect("canonical identity types are always serializable");
            value
                .as_object_mut()
                .expect("identity is an object")
                .remove("services");
            serde_json::to_string(&value).expect("canonical identity JSON")
        } else {
            serde_json::to_string(&normalized)
                .expect("canonical identity types are always serializable")
        };
        let digest = sha256_hex(canonical_json.as_bytes());
        Ok(Self {
            schema_version: normalized.schema_version,
            canonicalization_version: normalized.canonicalization_version,
            canonical_json,
            digest,
        })
    }
}

impl ResolvedEnvironmentIdentityInputV1 {
    pub fn canonicalized(mut self) -> Result<Self, EnvironmentIdentityError> {
        if !matches!(self.schema_version, 1 | 2) {
            return Err(EnvironmentIdentityError::UnsupportedVersion {
                kind: "environment identity schema",
                version: self.schema_version,
            });
        }
        if !matches!(self.canonicalization_version, 1 | 2) {
            return Err(EnvironmentIdentityError::UnsupportedVersion {
                kind: "environment identity canonicalization",
                version: self.canonicalization_version,
            });
        }
        if self.schema_version != self.canonicalization_version {
            let (kind, version) = if self.schema_version > self.canonicalization_version {
                ("environment identity schema", self.schema_version)
            } else {
                (
                    "environment identity canonicalization",
                    self.canonicalization_version,
                )
            };
            return Err(EnvironmentIdentityError::UnsupportedVersion { kind, version });
        }
        if self.schema_version == 1 && !self.services.is_empty() {
            return Err(EnvironmentIdentityError::UnsupportedVersion {
                kind: "environment identity schema",
                version: self.schema_version,
            });
        }
        supported(
            "final verification plan",
            self.plan.version,
            FINAL_VERIFICATION_PLAN_VERSION_V1,
        )?;
        supported(
            "verification input manifest",
            self.input_manifest.version,
            VERIFICATION_INPUT_MANIFEST_VERSION_V1,
        )?;
        if self.plan.hermeticity.reusable
            && (!self.plan.hermeticity.hermetic || self.plan.hermeticity.network_access)
        {
            return Err(EnvironmentIdentityError::InvalidReusableHermeticity);
        }
        nonempty("profile_id", &self.plan.profile_id)?;
        if self.plan.profile_revision == 0 {
            return Err(EnvironmentIdentityError::MissingResolved {
                field: "profile_revision",
            });
        }
        if self.plan.commands.is_empty() {
            return Err(EnvironmentIdentityError::MissingResolved { field: "commands" });
        }
        canonicalize_selection(&mut self.selection)?;
        nonempty("image reference", &self.image.reference)?;
        validate_digest("image", &self.image.digest)?;
        nonempty("runner_version", &self.runner_version)?;
        nonempty("target", &self.target)?;

        let mut command_ids = BTreeSet::new();
        for command in &mut self.plan.commands {
            nonempty("command check_id", &command.check_id)?;
            nonempty("command executable", &command.executable)?;
            if command.descriptor_revision == 0 {
                return Err(EnvironmentIdentityError::MissingResolved {
                    field: "descriptor_revision",
                });
            }
            normalize_strings(&mut command.environment_names, "command environment name")?;
            if !command_ids.insert(command.check_id.clone()) {
                return Err(EnvironmentIdentityError::Duplicate {
                    kind: "command check ID",
                    value: command.check_id.clone(),
                });
            }
        }
        normalize_strings(&mut self.plan.required_checks, "required check")?;
        for check in &self.plan.required_checks {
            if !command_ids.contains(check) {
                return Err(EnvironmentIdentityError::UncoveredRequiredCheck {
                    check_id: check.clone(),
                });
            }
        }

        normalize_strings(&mut self.input_manifest.repo_paths, "manifest repo path")?;
        normalize_strings(
            &mut self.input_manifest.environment_names,
            "manifest environment name",
        )?;
        normalize_strings(
            &mut self.input_manifest.output_only_globs,
            "output-only glob",
        )?;
        let input_paths: BTreeSet<_> = self.input_manifest.repo_paths.iter().collect();
        for output in &self.input_manifest.output_only_globs {
            if input_paths.contains(output) {
                return Err(EnvironmentIdentityError::AmbiguousDeclaration {
                    detail: format!("{output} is both an input path and output-only glob"),
                });
            }
        }
        sort_unique_by(
            &mut self.input_manifest.read_only_external_inputs,
            |entry| entry.id.clone(),
            "external input ID",
        )?;
        for entry in &self.input_manifest.read_only_external_inputs {
            nonempty("external input ID", &entry.id)?;
            nonempty("external input locator", &entry.locator)?;
        }

        sort_unique_by(
            &mut self.tool_probes,
            |probe| probe.tool.clone(),
            "tool probe",
        )?;
        for probe in &self.tool_probes {
            nonempty("tool probe tool", &probe.tool)?;
            nonempty("tool probe version", &probe.version)?;
            validate_digest("tool executable", &probe.executable_digest)?;
            if probe.status != ToolProbeStatus::Passed {
                return Err(EnvironmentIdentityError::FailedToolProbe {
                    tool: probe.tool.clone(),
                });
            }
        }
        for command in &self.plan.commands {
            if !self
                .tool_probes
                .iter()
                .any(|probe| probe.tool == command.executable)
            {
                return Err(EnvironmentIdentityError::MissingToolProbe {
                    executable: command.executable.clone(),
                });
            }
        }
        sort_unique_by(
            &mut self.lockfile_digests,
            |entry| entry.path.clone(),
            "lockfile path",
        )?;
        for lockfile in &self.lockfile_digests {
            nonempty("lockfile path", &lockfile.path)?;
            validate_digest("lockfile", &lockfile.digest)?;
        }
        normalize_strings(&mut self.features, "feature")?;
        for (name, value) in &self.allowlisted_environment {
            nonempty("allowlisted environment name", name)?;
            nonempty("allowlisted environment value", value)?;
        }
        sort_unique_by(
            &mut self.services,
            |service| service.preset_id.clone(),
            "service preset ID",
        )?;
        let (mut types, mut ports, mut env_names) =
            (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
        for service in &mut self.services {
            nonempty("service preset ID", &service.preset_id)?;
            nonempty("service type", &service.service_type)?;
            nonempty("service image reference", &service.image_reference)?;
            validate_digest("service image", &service.image_digest)?;
            validate_digest(
                "effective service configuration",
                &service.effective_configuration_digest,
            )?;
            if service.port <= 0 || service.verification_protocol_revision == 0 {
                return Err(EnvironmentIdentityError::MissingResolved {
                    field: "service port or protocol revision",
                });
            }
            normalize_strings(
                &mut service.exported_environment_names,
                "service exported environment name",
            )?;
            if !types.insert(service.service_type.clone()) {
                return Err(EnvironmentIdentityError::Duplicate {
                    kind: "service type",
                    value: service.service_type.clone(),
                });
            }
            if !ports.insert(service.port) {
                return Err(EnvironmentIdentityError::Duplicate {
                    kind: "service port",
                    value: service.port.to_string(),
                });
            }
            for name in &service.exported_environment_names {
                if !env_names.insert(name.clone()) {
                    return Err(EnvironmentIdentityError::Duplicate {
                        kind: "service exported environment name",
                        value: name.clone(),
                    });
                }
            }
        }
        Ok(self)
    }
}

fn canonicalize_selection(
    selection: &mut ResolvedVerificationSelectionV1,
) -> Result<(), EnvironmentIdentityError> {
    let ResolvedVerificationSelectionV1::SelectedGroups {
        selection_rules,
        selected_command_groups,
    } = selection
    else {
        return Ok(());
    };
    if selection_rules.is_empty() {
        return Err(EnvironmentIdentityError::MissingResolved {
            field: "selection_rules",
        });
    }
    if selected_command_groups.is_empty() {
        return Err(EnvironmentIdentityError::MissingResolved {
            field: "selected_command_groups",
        });
    }

    let mut configured_groups = BTreeSet::new();
    for rule in selection_rules {
        normalize_strings(&mut rule.matches, "selection rule match")?;
        if rule.matches.is_empty() {
            return Err(EnvironmentIdentityError::MissingResolved {
                field: "selection rule matches",
            });
        }
        normalize_strings(&mut rule.command_groups, "selection rule command group")?;
        if rule.command_groups.is_empty() {
            return Err(EnvironmentIdentityError::MissingResolved {
                field: "selection rule command_groups",
            });
        }
        configured_groups.extend(rule.command_groups.iter().cloned());
    }
    normalize_strings(selected_command_groups, "selected command group")?;
    for group in selected_command_groups {
        if !configured_groups.contains(group) {
            return Err(EnvironmentIdentityError::UnknownSelectedCommandGroup {
                group: group.clone(),
            });
        }
    }
    Ok(())
}

fn supported(
    kind: &'static str,
    actual: u32,
    expected: u32,
) -> Result<(), EnvironmentIdentityError> {
    if actual == expected {
        Ok(())
    } else {
        Err(EnvironmentIdentityError::UnsupportedVersion {
            kind,
            version: actual,
        })
    }
}

fn nonempty(field: &'static str, value: &str) -> Result<(), EnvironmentIdentityError> {
    if value.trim().is_empty() {
        Err(EnvironmentIdentityError::MissingResolved { field })
    } else {
        Ok(())
    }
}

fn validate_digest(kind: &'static str, digest: &str) -> Result<(), EnvironmentIdentityError> {
    let valid = digest.len() == 71
        && digest.starts_with("sha256:")
        && digest[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if valid {
        Ok(())
    } else {
        Err(EnvironmentIdentityError::MalformedDigest {
            kind,
            value: digest.to_owned(),
        })
    }
}

fn normalize_strings(
    values: &mut [String],
    kind: &'static str,
) -> Result<(), EnvironmentIdentityError> {
    values.sort();
    for value in values.iter() {
        nonempty(kind, value)?;
    }
    if let Some(duplicate) = values.windows(2).find(|pair| pair[0] == pair[1]) {
        return Err(EnvironmentIdentityError::Duplicate {
            kind,
            value: duplicate[0].clone(),
        });
    }
    Ok(())
}

fn sort_unique_by<T>(
    values: &mut [T],
    key: impl Fn(&T) -> String,
    kind: &'static str,
) -> Result<(), EnvironmentIdentityError> {
    values.sort_by_key(&key);
    for pair in values.windows(2) {
        if key(&pair[0]) == key(&pair[1]) {
            return Err(EnvironmentIdentityError::Duplicate {
                kind,
                value: key(&pair[0]),
            });
        }
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn input() -> ResolvedEnvironmentIdentityInputV1 {
        ResolvedEnvironmentIdentityInputV1 {
            schema_version: 1,
            canonicalization_version: 1,
            plan: CanonicalFinalVerificationPlanV1 {
                version: 1,
                profile_id: "ci".into(),
                profile_revision: 3,
                commands: vec![CanonicalCommandDescriptorV1 {
                    check_id: "test".into(),
                    executable: "cargo".into(),
                    argv: vec!["test".into()],
                    working_directory: ".".into(),
                    environment_names: vec!["RUSTFLAGS".into(), "CI".into()],
                    timeout_seconds: 60,
                    descriptor_revision: 1,
                }],
                required_checks: vec!["test".into()],
                hermeticity: CanonicalHermeticityV1 {
                    hermetic: true,
                    reusable: true,
                    network_access: false,
                },
            },
            selection: ResolvedVerificationSelectionV1::legacy_flat_plan(),
            input_manifest: VerificationInputManifestV1 {
                version: 1,
                repo_paths: vec!["Cargo.lock".into(), "Cargo.toml".into()],
                environment_names: vec!["CI".into()],
                read_only_external_inputs: vec![DeclaredExternalInputV1 {
                    id: "registry".into(),
                    locator: "https://registry.example".into(),
                }],
                output_only_globs: vec!["target/**".into()],
            },
            image: ImmutableImageV1 {
                reference: "rust:1.85".into(),
                digest: DIGEST.into(),
            },
            tool_probes: vec![ToolProbeV1 {
                tool: "cargo".into(),
                version: "1.85.0".into(),
                executable_digest: DIGEST.into(),
                status: ToolProbeStatus::Passed,
            }],
            runner_version: "runner-1".into(),
            lockfile_digests: vec![LockfileDigestV1 {
                path: "Cargo.lock".into(),
                digest: DIGEST.into(),
            }],
            target: "x86_64-unknown-linux-gnu".into(),
            features: vec!["serde".into(), "sqlx".into()],
            allowlisted_environment: BTreeMap::from([("CI".into(), "true".into())]),
            services: Vec::new(),
        }
    }

    #[test]
    fn v1_omits_additive_services_field_from_persisted_json() {
        let identity = EnvironmentIdentityV1::derive(input()).unwrap();
        assert!(!identity.canonical_json.contains("\"services\""));
    }

    #[test]
    fn v2_service_identity_is_sorted_and_secret_free() {
        let mut v2 = input();
        v2.schema_version = 2;
        v2.canonicalization_version = 2;
        v2.services = vec![ResolvedCatalogServiceIdentityV1 {
            preset_id: "postgres".into(),
            service_type: "postgres".into(),
            image_reference: "registry.example/postgres:18".into(),
            image_digest: DIGEST.into(),
            port: 5432,
            exported_environment_names: vec!["DATABASE_URL".into()],
            verification_protocol_revision: 1,
            effective_configuration_digest: DIGEST.into(),
        }];
        let identity = EnvironmentIdentityV1::derive(v2).unwrap();
        assert!(identity.canonical_json.contains("\"services\""));
        assert!(!identity.canonical_json.contains("POSTGRES_PASSWORD"));
    }

    fn catalog_service() -> ResolvedCatalogServiceIdentityV1 {
        ResolvedCatalogServiceIdentityV1 {
            preset_id: "postgres".into(),
            service_type: "postgres".into(),
            image_reference: "registry.example/postgres:18".into(),
            image_digest: DIGEST.into(),
            port: 5432,
            exported_environment_names: vec!["TEST_POSTGRES_URL".into(), "DATABASE_URL".into()],
            verification_protocol_revision: 1,
            effective_configuration_digest: DIGEST.into(),
        }
    }

    #[test]
    fn v2_catalog_service_changes_cause_identity_misses_without_secret_values() {
        let mut base = input();
        base.schema_version = 2;
        base.canonicalization_version = 2;
        base.services = vec![catalog_service()];
        let baseline = EnvironmentIdentityV1::derive(base.clone()).unwrap();
        assert!(!baseline.canonical_json.contains("postgres-password"));

        let mut changed_digest = base.clone();
        changed_digest.services[0].image_digest = alternate_digest('4');
        assert_ne!(
            baseline.digest,
            EnvironmentIdentityV1::derive(changed_digest)
                .unwrap()
                .digest
        );

        let mut changed_configuration = base.clone();
        changed_configuration.services[0].effective_configuration_digest = alternate_digest('5');
        assert_ne!(
            baseline.digest,
            EnvironmentIdentityV1::derive(changed_configuration)
                .unwrap()
                .digest
        );

        let mut changed_catalog = base;
        let mut redis = catalog_service();
        redis.preset_id = "redis".into();
        redis.service_type = "redis".into();
        redis.port = 6379;
        redis.exported_environment_names = vec!["REDIS_URL".into()];
        changed_catalog.services.push(redis);
        assert_ne!(
            baseline.digest,
            EnvironmentIdentityV1::derive(changed_catalog)
                .unwrap()
                .digest
        );
    }

    #[test]
    fn set_permutations_are_normalized_but_command_order_is_preserved() {
        let mut unordered = input();
        unordered.plan.commands.push(CanonicalCommandDescriptorV1 {
            check_id: "lint".into(),
            executable: "clippy".into(),
            argv: vec!["--all-targets".into()],
            working_directory: ".".into(),
            environment_names: vec![],
            timeout_seconds: 60,
            descriptor_revision: 1,
        });
        unordered.plan.required_checks.push("lint".into());
        unordered
            .input_manifest
            .environment_names
            .push("TERM".into());
        unordered
            .input_manifest
            .read_only_external_inputs
            .push(DeclaredExternalInputV1 {
                id: "mirror".into(),
                locator: "https://mirror.example".into(),
            });
        unordered
            .input_manifest
            .output_only_globs
            .push("reports/**".into());
        unordered.tool_probes.push(ToolProbeV1 {
            tool: "clippy".into(),
            version: "1.85.0".into(),
            executable_digest: DIGEST.into(),
            status: ToolProbeStatus::Passed,
        });
        unordered.lockfile_digests.push(LockfileDigestV1 {
            path: "pnpm-lock.yaml".into(),
            digest: DIGEST.into(),
        });
        let first = EnvironmentIdentityV1::derive(unordered.clone()).unwrap();
        let mut permuted = unordered;
        permuted.plan.commands[0].environment_names.reverse();
        permuted.plan.required_checks.reverse();
        permuted.input_manifest.repo_paths.reverse();
        permuted.input_manifest.environment_names.reverse();
        permuted.input_manifest.read_only_external_inputs.reverse();
        permuted.input_manifest.output_only_globs.reverse();
        permuted.tool_probes.reverse();
        permuted.lockfile_digests.reverse();
        permuted.features.reverse();
        let second = EnvironmentIdentityV1::derive(permuted).unwrap();
        assert_eq!(first, second);
        let mut reordered = input();
        reordered.plan.commands.push(CanonicalCommandDescriptorV1 {
            check_id: "lint".into(),
            executable: "cargo".into(),
            argv: vec!["clippy".into()],
            working_directory: ".".into(),
            environment_names: vec![],
            timeout_seconds: 60,
            descriptor_revision: 1,
        });
        let a = EnvironmentIdentityV1::derive(reordered.clone()).unwrap();
        reordered.plan.commands.reverse();
        assert_ne!(
            a.digest,
            EnvironmentIdentityV1::derive(reordered).unwrap().digest
        );
    }

    #[test]
    fn every_identity_dimension_changes_digest() {
        let baseline = EnvironmentIdentityV1::derive(input()).unwrap();
        let variants: Vec<(&str, Box<dyn Fn(&mut ResolvedEnvironmentIdentityInputV1)>)> = vec![
            (
                "profile ID",
                Box::new(|v| v.plan.profile_id = "strict-ci".into()),
            ),
            (
                "profile revision",
                Box::new(|v| v.plan.profile_revision = 4),
            ),
            (
                "command check ID",
                Box::new(|v| {
                    v.plan.commands[0].check_id = "unit".into();
                    v.plan.required_checks[0] = "unit".into();
                }),
            ),
            (
                "command executable",
                Box::new(|v| {
                    v.plan.commands[0].executable = "nextest".into();
                    v.tool_probes[0].tool = "nextest".into();
                }),
            ),
            (
                "command argv",
                Box::new(|v| v.plan.commands[0].argv.push("--locked".into())),
            ),
            (
                "command working directory",
                Box::new(|v| v.plan.commands[0].working_directory = "server".into()),
            ),
            (
                "command environment names",
                Box::new(|v| v.plan.commands[0].environment_names.push("TERM".into())),
            ),
            (
                "command timeout",
                Box::new(|v| v.plan.commands[0].timeout_seconds = 61),
            ),
            (
                "command descriptor revision",
                Box::new(|v| v.plan.commands[0].descriptor_revision = 2),
            ),
            (
                "required checks",
                Box::new(|v| v.plan.required_checks.clear()),
            ),
            (
                "hermetic",
                Box::new(|v| {
                    v.plan.hermeticity.hermetic = false;
                    v.plan.hermeticity.reusable = false;
                }),
            ),
            (
                "reusable",
                Box::new(|v| v.plan.hermeticity.reusable = false),
            ),
            (
                "network access",
                Box::new(|v| {
                    v.plan.hermeticity.network_access = true;
                    v.plan.hermeticity.reusable = false;
                }),
            ),
            (
                "manifest repo paths",
                Box::new(|v| v.input_manifest.repo_paths.push("src".into())),
            ),
            (
                "manifest environment names",
                Box::new(|v| v.input_manifest.environment_names.push("TERM".into())),
            ),
            (
                "external input ID",
                Box::new(|v| v.input_manifest.read_only_external_inputs[0].id = "mirror".into()),
            ),
            (
                "external input locator",
                Box::new(|v| {
                    v.input_manifest.read_only_external_inputs[0].locator =
                        "https://mirror.example".into()
                }),
            ),
            (
                "output-only globs",
                Box::new(|v| v.input_manifest.output_only_globs.push("reports/**".into())),
            ),
            (
                "image reference",
                Box::new(|v| v.image.reference = "rust:1.86".into()),
            ),
            (
                "image digest",
                Box::new(|v| v.image.digest = alternate_digest('1')),
            ),
            (
                "tool probe version",
                Box::new(|v| v.tool_probes[0].version = "1.86.0".into()),
            ),
            (
                "tool executable digest",
                Box::new(|v| v.tool_probes[0].executable_digest = alternate_digest('2')),
            ),
            (
                "runner version",
                Box::new(|v| v.runner_version = "runner-2".into()),
            ),
            (
                "lockfile path",
                Box::new(|v| v.lockfile_digests[0].path = "pnpm-lock.yaml".into()),
            ),
            (
                "lockfile digest",
                Box::new(|v| v.lockfile_digests[0].digest = alternate_digest('3')),
            ),
            (
                "target",
                Box::new(|v| v.target = "aarch64-unknown-linux-gnu".into()),
            ),
            ("features", Box::new(|v| v.features.push("extra".into()))),
            (
                "allowlisted environment",
                Box::new(|v| {
                    v.allowlisted_environment
                        .insert("CI".into(), "false".into());
                }),
            ),
        ];
        for (dimension, mutate) in variants {
            let mut variant = input();
            mutate(&mut variant);
            let value = EnvironmentIdentityV1::derive(variant).unwrap_or_else(|error| {
                panic!("{dimension} unexpectedly failed validation: {error}")
            });
            assert_ne!(baseline.canonical_json, value.canonical_json, "{dimension}");
            assert_ne!(baseline.digest, value.digest, "{dimension}");
        }
    }

    #[test]
    fn ordered_descriptor_add_remove_reorder_and_argv_changes_never_share_identity() {
        let baseline = EnvironmentIdentityV1::derive(input()).expect("baseline identity");
        let additional = CanonicalCommandDescriptorV1 {
            check_id: "lint".into(),
            executable: "cargo".into(),
            argv: vec!["clippy".into(), "--all-targets".into()],
            working_directory: ".".into(),
            environment_names: Vec::new(),
            timeout_seconds: 60,
            descriptor_revision: 1,
        };

        let mut added = input();
        added.plan.commands.push(additional);
        added.plan.required_checks.push("lint".into());
        let added_identity = EnvironmentIdentityV1::derive(added.clone()).expect("added identity");
        assert_ne!(
            baseline.digest, added_identity.digest,
            "adding a descriptor"
        );

        added.plan.commands.reverse();
        let reordered_identity = EnvironmentIdentityV1::derive(added).expect("reordered identity");
        assert_ne!(
            added_identity.digest, reordered_identity.digest,
            "descriptor order is semantic"
        );

        let mut argv_changed = input();
        argv_changed.plan.commands[0].argv.push("--locked".into());
        assert_ne!(
            baseline.digest,
            EnvironmentIdentityV1::derive(argv_changed)
                .expect("argv changed identity")
                .digest,
            "argv is semantic"
        );

        let removed_identity = EnvironmentIdentityV1::derive(input()).expect("removed identity");
        assert_ne!(
            added_identity.digest, removed_identity.digest,
            "removing a descriptor"
        );
        assert_eq!(baseline.digest, removed_identity.digest);
    }

    fn alternate_digest(hex: char) -> String {
        format!("sha256:{}", hex.to_string().repeat(64))
    }

    #[test]
    fn rejects_unresolved_mutable_or_ambiguous_inputs() {
        let mut bad = input();
        bad.plan.required_checks.push("missing".into());
        assert!(matches!(
            EnvironmentIdentityV1::derive(bad),
            Err(EnvironmentIdentityError::UncoveredRequiredCheck { .. })
        ));
        let mut bad = input();
        bad.image.digest = "rust:latest".into();
        assert!(matches!(
            EnvironmentIdentityV1::derive(bad),
            Err(EnvironmentIdentityError::MalformedDigest { .. })
        ));
        let mut bad = input();
        bad.tool_probes[0].status = ToolProbeStatus::Failed;
        assert!(matches!(
            EnvironmentIdentityV1::derive(bad),
            Err(EnvironmentIdentityError::FailedToolProbe { .. })
        ));
        let mut bad = input();
        bad.input_manifest
            .output_only_globs
            .push("Cargo.lock".into());
        assert!(matches!(
            EnvironmentIdentityV1::derive(bad),
            Err(EnvironmentIdentityError::AmbiguousDeclaration { .. })
        ));
    }

    #[test]
    fn rejects_reusable_non_hermetic_plan_before_identity_derivation() {
        let mut bad = input();
        bad.plan.hermeticity.hermetic = false;

        assert_eq!(
            bad.clone().canonicalized(),
            Err(EnvironmentIdentityError::InvalidReusableHermeticity)
        );
        assert_eq!(
            EnvironmentIdentityV1::derive(bad),
            Err(EnvironmentIdentityError::InvalidReusableHermeticity)
        );
    }

    #[test]
    fn rejects_reusable_networked_plan_before_identity_derivation() {
        let mut bad = input();
        bad.plan.hermeticity.network_access = true;

        assert_eq!(
            bad.clone().canonicalized(),
            Err(EnvironmentIdentityError::InvalidReusableHermeticity)
        );
        assert_eq!(
            EnvironmentIdentityV1::derive(bad),
            Err(EnvironmentIdentityError::InvalidReusableHermeticity)
        );
    }

    #[test]
    fn selection_rules_and_selected_groups_bind_identity() {
        let selection = ResolvedVerificationSelectionV1::SelectedGroups {
            selection_rules: selection_rules_for_test(),
            selected_command_groups: vec!["shared".into(), "server".into()],
        };
        let mut equivalent = input();
        equivalent.selection = selection.clone();
        let baseline = EnvironmentIdentityV1::derive(equivalent.clone()).unwrap();
        let ResolvedVerificationSelectionV1::SelectedGroups {
            selection_rules,
            selected_command_groups,
        } = &mut equivalent.selection
        else {
            unreachable!()
        };
        selection_rules[0].matches.reverse();
        selection_rules[0].command_groups.reverse();
        selected_command_groups.reverse();
        assert_eq!(baseline, EnvironmentIdentityV1::derive(equivalent).unwrap());

        let mut reordered_rules = input();
        reordered_rules.selection = selection;
        let ResolvedVerificationSelectionV1::SelectedGroups {
            selection_rules, ..
        } = &mut reordered_rules.selection
        else {
            unreachable!()
        };
        selection_rules.reverse();
        assert_ne!(
            baseline.digest,
            EnvironmentIdentityV1::derive(reordered_rules)
                .unwrap()
                .digest
        );

        let mut different_groups = input();
        different_groups.selection = ResolvedVerificationSelectionV1::SelectedGroups {
            selection_rules: selection_rules_for_test(),
            selected_command_groups: vec!["ui".into()],
        };
        assert_ne!(
            baseline.digest,
            EnvironmentIdentityV1::derive(different_groups)
                .unwrap()
                .digest
        );
    }

    fn selection_rules_for_test() -> Vec<ResolvedVerificationSelectionRuleV1> {
        vec![
            ResolvedVerificationSelectionRuleV1 {
                matches: vec!["server/**".into(), "Cargo.toml".into()],
                command_groups: vec!["server".into(), "shared".into()],
            },
            ResolvedVerificationSelectionRuleV1 {
                matches: vec!["ui/**".into()],
                command_groups: vec!["ui".into()],
            },
        ]
    }

    #[test]
    fn selection_rejects_empty_or_unknown_selected_groups() {
        let mut empty = input();
        empty.selection = ResolvedVerificationSelectionV1::SelectedGroups {
            selection_rules: selection_rules_for_test(),
            selected_command_groups: Vec::new(),
        };
        assert!(matches!(
            EnvironmentIdentityV1::derive(empty),
            Err(EnvironmentIdentityError::MissingResolved {
                field: "selected_command_groups"
            })
        ));

        let mut unknown = input();
        unknown.selection = ResolvedVerificationSelectionV1::SelectedGroups {
            selection_rules: selection_rules_for_test(),
            selected_command_groups: vec!["unknown".into()],
        };
        assert!(matches!(
            EnvironmentIdentityV1::derive(unknown),
            Err(EnvironmentIdentityError::UnknownSelectedCommandGroup { .. })
        ));
    }

    #[test]
    fn derives_reusable_hermetic_network_denied_plan() {
        let identity = EnvironmentIdentityV1::derive(input()).unwrap();

        assert!(!identity.canonical_json.is_empty());
        assert!(!identity.digest.is_empty());
    }

    #[test]
    fn supported_version_boundary_rejects_every_unknown_version() {
        let mut bad = input();
        bad.schema_version = 2;
        assert!(matches!(
            EnvironmentIdentityV1::derive(bad),
            Err(EnvironmentIdentityError::UnsupportedVersion {
                kind: "environment identity schema",
                ..
            })
        ));
        let mut bad = input();
        bad.canonicalization_version = 2;
        assert!(matches!(
            EnvironmentIdentityV1::derive(bad),
            Err(EnvironmentIdentityError::UnsupportedVersion {
                kind: "environment identity canonicalization",
                ..
            })
        ));
        let mut bad = input();
        bad.plan.version = 2;
        assert!(matches!(
            EnvironmentIdentityV1::derive(bad),
            Err(EnvironmentIdentityError::UnsupportedVersion {
                kind: "final verification plan",
                ..
            })
        ));
        let mut bad = input();
        bad.input_manifest.version = 2;
        assert!(matches!(
            EnvironmentIdentityV1::derive(bad),
            Err(EnvironmentIdentityError::UnsupportedVersion {
                kind: "verification input manifest",
                ..
            })
        ));
    }
}

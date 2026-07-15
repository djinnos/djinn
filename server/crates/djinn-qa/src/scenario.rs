use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{Profile, Taxonomy};

/// A validated scenario inventory, ordered by stable scenario identifier.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ScenarioInventory {
    pub version: u32,
    pub scenarios: Vec<Scenario>,
}

fn scenario_yaml_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), ScenarioError> {
    let entries = fs::read_dir(dir).map_err(|source| ScenarioError::Read {
        path: dir.display().to_string(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| ScenarioError::Read {
            path: dir.display().to_string(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() {
            scenario_yaml_files(&path, files)?;
        } else if matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("yaml" | "yml")
        ) {
            files.push(path);
        }
    }
    Ok(())
}

/// Metadata consumed by the deterministic QA runner.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Scenario {
    pub id: String,
    pub version: u32,
    pub enabled: bool,
    pub profiles: Vec<Profile>,
    pub sources: Vec<SourceIdentifier>,
    pub primary_coverage: String,
    pub secondary_coverage: Vec<String>,
    pub execution: Execution,
    pub isolation: Isolation,
    pub watch_paths: Vec<String>,
    pub blocked_dependency: Option<String>,
}

/// A durable memory or incident identifier that motivated a scenario.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceIdentifier {
    pub kind: SourceKind,
    pub id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    Memory,
    Incident,
}

/// An executable target. `cargo-package` resolves a package relative to the supplied repository root.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Execution {
    CargoPackage {
        package: String,
        #[serde(default)]
        test: Option<String>,
    },
}

/// Isolation declarations for runner resources and smoke-CI safety.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Isolation {
    pub database: IsolationMode,
    pub providers: IsolationMode,
    pub channel: IsolationMode,
    #[serde(default)]
    pub live_credentials: bool,
    #[serde(default)]
    pub live_providers: bool,
    #[serde(default)]
    pub kubernetes: bool,
    #[serde(default)]
    pub external_network: bool,
    #[serde(default)]
    pub wall_clock_sleep: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IsolationMode {
    Isolated,
    Shared,
}

impl ScenarioInventory {
    pub fn from_yaml(yaml: &str) -> Result<Self, ScenarioError> {
        let raw: RawInventory = serde_yaml::from_str(yaml).map_err(ScenarioError::Yaml)?;
        if raw.version != 1 {
            return Err(ScenarioError::UnsupportedVersion {
                version: raw.version,
            });
        }
        let mut scenarios = Vec::with_capacity(raw.scenarios.len());
        for raw in raw.scenarios {
            let profiles = raw
                .profiles
                .into_iter()
                .map(|profile| {
                    profile.parse().map_err(|_| ScenarioError::InvalidProfile {
                        scenario: raw.id.clone(),
                        value: profile,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            scenarios.push(Scenario {
                id: raw.id,
                version: raw.version,
                enabled: raw.enabled,
                profiles,
                sources: raw.sources,
                primary_coverage: raw.primary_coverage,
                secondary_coverage: raw.secondary_coverage,
                execution: raw.execution,
                isolation: raw.isolation,
                watch_paths: raw.watch_paths,
                blocked_dependency: raw.blocked_dependency,
            });
        }
        scenarios.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(Self {
            version: raw.version,
            scenarios,
        })
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ScenarioError> {
        let path = path.as_ref();
        if path.is_dir() {
            return Self::load_directory(path);
        }
        let yaml = fs::read_to_string(path).map_err(|source| ScenarioError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_yaml(&yaml)
    }

    /// Merge version-one scenario files from a theme directory deterministically.
    fn load_directory(path: &Path) -> Result<Self, ScenarioError> {
        let mut files = Vec::new();
        scenario_yaml_files(path, &mut files)?;
        files.sort();
        let mut scenarios = Vec::new();
        let mut scenario_files = BTreeMap::new();
        let mut primary_files = BTreeMap::new();
        for file in files {
            let file_name = file.display().to_string();
            for scenario in Self::load(&file)?.scenarios {
                if let Some(first_path) =
                    scenario_files.insert(scenario.id.clone(), file_name.clone())
                {
                    return Err(ScenarioError::DuplicateDirectoryRegistration {
                        registration: format!("scenario id `{}`", scenario.id),
                        first_path,
                        duplicate_path: file_name,
                    });
                }
                if let Some(first_path) =
                    primary_files.insert(scenario.primary_coverage.clone(), file_name.clone())
                {
                    return Err(ScenarioError::DuplicateDirectoryRegistration {
                        registration: format!("primary coverage `{}`", scenario.primary_coverage),
                        first_path,
                        duplicate_path: file_name,
                    });
                }
                scenarios.push(scenario);
            }
        }
        scenarios.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(Self {
            version: 1,
            scenarios,
        })
    }

    /// Validate cross-inventory invariants and local targets against an explicit repository root.
    /// Diagnostics are sorted by scenario ID and then message, independent of YAML input order.
    pub fn validate(
        &self,
        taxonomy: &Taxonomy,
        repository_root: impl AsRef<Path>,
    ) -> Result<(), ScenarioValidationErrors> {
        let repository_root = repository_root.as_ref();
        let known_coverage = taxonomy
            .coverage
            .iter()
            .map(|entry| entry.id.as_ref())
            .collect::<BTreeSet<_>>();
        let mut diagnostics = Vec::new();
        let mut ids = BTreeSet::new();
        let mut primaries = BTreeMap::<&str, &str>::new();
        for scenario in &self.scenarios {
            let prefix = format!("scenario `{}`", scenario.id);
            if !ids.insert(scenario.id.as_str()) {
                diagnostics.push(format!("{prefix}: duplicate scenario id"));
            }
            if scenario.id.is_empty() {
                diagnostics.push("scenario ``: id must not be empty".into());
            }
            if scenario.version == 0 {
                diagnostics.push(format!("{prefix}: version must be greater than zero"));
            }
            if scenario.profiles.is_empty() {
                diagnostics.push(format!("{prefix}: profile eligibility must not be empty"));
            }
            if scenario.sources.is_empty() {
                diagnostics.push(format!(
                    "{prefix}: at least one source identifier is required"
                ));
            }
            for source in &scenario.sources {
                if source.id.trim().is_empty() {
                    diagnostics.push(format!("{prefix}: source identifier must not be empty"));
                }
            }
            if scenario.watch_paths.is_empty() {
                diagnostics.push(format!(
                    "{prefix}: at least one staleness watch path is required"
                ));
            }
            validate_coverage(
                &known_coverage,
                &prefix,
                "primary",
                &scenario.primary_coverage,
                &mut diagnostics,
            );
            if let Some(previous) = primaries.insert(&scenario.primary_coverage, &scenario.id) {
                diagnostics.push(format!("{prefix}: primary coverage `{}` is already primary-registered by scenario `{previous}`", scenario.primary_coverage));
            }
            for coverage in &scenario.secondary_coverage {
                validate_coverage(
                    &known_coverage,
                    &prefix,
                    "secondary",
                    coverage,
                    &mut diagnostics,
                );
            }
            if scenario.profiles.contains(&Profile::SmokeCi) {
                let isolation = &scenario.isolation;
                for (name, requested) in [
                    ("live credentials", isolation.live_credentials),
                    ("live providers", isolation.live_providers),
                    ("Kubernetes", isolation.kubernetes),
                    ("external network access", isolation.external_network),
                    ("wall-clock sleep dependency", isolation.wall_clock_sleep),
                ] {
                    if requested {
                        diagnostics.push(format!(
                            "{prefix}: smoke-ci scenario may not request {name}"
                        ));
                    }
                }
            }
            if scenario.blocked_dependency.is_none()
                && !resolves(&scenario.execution, repository_root)
            {
                diagnostics.push(format!("{prefix}: executable target cannot be resolved from repository root `{}`; declare blocked_dependency explicitly while it is unavailable", repository_root.display()));
            }
        }
        diagnostics.sort();
        diagnostics.dedup();
        if diagnostics.is_empty() {
            Ok(())
        } else {
            Err(ScenarioValidationErrors(diagnostics))
        }
    }
}

fn validate_coverage(
    known: &BTreeSet<&str>,
    prefix: &str,
    role: &str,
    coverage: &str,
    diagnostics: &mut Vec<String>,
) {
    if coverage.is_empty() {
        diagnostics.push(format!("{prefix}: {role} coverage id is required"));
    } else if !known.contains(coverage) {
        diagnostics.push(format!(
            "{prefix}: {role} coverage id `{coverage}` is not present in taxonomy"
        ));
    }
}

fn resolves(execution: &Execution, root: &Path) -> bool {
    match execution {
        Execution::CargoPackage { package, test } => {
            let manifests = cargo_manifests(root);
            let package_manifest = manifests
                .iter()
                .find(|manifest| package_name(manifest).as_deref() == Some(package));
            match (package_manifest, test) {
                (Some(_), None) => true,
                (Some(manifest), Some(test)) => manifest
                    .parent()
                    .is_some_and(|dir| dir.join("tests").join(format!("{test}.rs")).is_file()),
                _ => false,
            }
        }
    }
}

fn cargo_manifests(root: &Path) -> Vec<PathBuf> {
    let mut manifests = Vec::new();
    visit(root, &mut manifests);
    manifests.sort();
    manifests
}

fn visit(dir: &Path, manifests: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|name| matches!(name.to_str(), Some("target" | ".git" | "node_modules")))
        {
            continue;
        }
        if path.is_dir() {
            visit(&path, manifests);
        } else if path.file_name().is_some_and(|name| name == "Cargo.toml") {
            manifests.push(path);
        }
    }
}

fn package_name(manifest: &Path) -> Option<String> {
    let content = fs::read_to_string(manifest).ok()?;
    let package = content.split("[package]").nth(1)?.split('[').next()?;
    package.lines().find_map(|line| {
        line.trim()
            .strip_prefix("name = ")
            .and_then(|value| value.trim().strip_prefix('"'))
            .and_then(|value| value.strip_suffix('"'))
            .map(ToOwned::to_owned)
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInventory {
    version: u32,
    scenarios: Vec<RawScenario>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScenario {
    id: String,
    version: u32,
    enabled: bool,
    profiles: Vec<String>,
    sources: Vec<SourceIdentifier>,
    primary_coverage: String,
    #[serde(default)]
    secondary_coverage: Vec<String>,
    execution: Execution,
    isolation: Isolation,
    watch_paths: Vec<String>,
    #[serde(default)]
    blocked_dependency: Option<String>,
}

#[derive(Debug, Error)]
pub enum ScenarioError {
    #[error("could not read scenario inventory `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("scenario inventory YAML is invalid: {0}")]
    Yaml(#[source] serde_yaml::Error),
    #[error("scenario inventory version `{version}` is unsupported; expected version `1`")]
    UnsupportedVersion { version: u32 },
    #[error("scenario `{scenario}` has invalid profile `{value}`; expected `smoke-ci`")]
    InvalidProfile { scenario: String, value: String },
    #[error(
        "scenario directory has duplicate {registration}: `{duplicate_path}` conflicts with `{first_path}`"
    )]
    DuplicateDirectoryRegistration {
        registration: String,
        first_path: String,
        duplicate_path: String,
    },
}

#[derive(Debug, Eq, PartialEq)]
pub struct ScenarioValidationErrors(pub Vec<String>);
impl fmt::Display for ScenarioValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("\n"))
    }
}
impl std::error::Error for ScenarioValidationErrors {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Taxonomy;

    fn taxonomy() -> Taxonomy {
        Taxonomy::from_yaml(include_str!("../tests/fixtures/valid-taxonomy.yaml")).unwrap()
    }
    fn inventory(name: &str) -> ScenarioInventory {
        ScenarioInventory::from_yaml(match name {
            "valid" => include_str!("../tests/fixtures/scenarios-valid.yaml"),
            "blocked" => include_str!("../tests/fixtures/scenarios-blocked.yaml"),
            "invalid" => include_str!("../tests/fixtures/scenarios-invalid.yaml"),
            _ => unreachable!(),
        })
        .unwrap()
    }
    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .to_path_buf()
    }

    #[test]
    fn accepts_resolved_and_explicitly_blocked_scenarios() {
        inventory("valid").validate(&taxonomy(), root()).unwrap();
        inventory("blocked").validate(&taxonomy(), root()).unwrap();
    }
    #[test]
    fn reports_each_cross_inventory_rejection_in_stable_order() {
        let error = inventory("invalid")
            .validate(&taxonomy(), root())
            .unwrap_err();
        assert_eq!(error.0, {
            let mut values = error.0.clone();
            values.sort();
            values
        });
        let rendered = error.to_string();
        for expected in [
            "duplicate scenario id",
            "at least one source identifier",
            "primary coverage `unknown.coverage`",
            "already primary-registered",
            "smoke-ci scenario may not request live credentials",
            "smoke-ci scenario may not request live providers",
            "smoke-ci scenario may not request Kubernetes",
            "smoke-ci scenario may not request external network access",
            "smoke-ci scenario may not request wall-clock sleep dependency",
            "executable target cannot be resolved",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected}: {rendered}"
            );
        }
    }

    fn directory_scenario(id: &str, primary: &str) -> String {
        format!(
            "version: 1\nscenarios:\n- id: {id}\n  version: 1\n  enabled: true\n  profiles: [smoke-ci]\n  sources: [{{kind: memory, id: fixture}}]\n  primary_coverage: {primary}\n  execution: {{kind: cargo-package, package: djinn-qa}}\n  isolation: {{database: isolated, providers: isolated, channel: isolated}}\n  watch_paths: [src/scenario.rs]\n"
        )
    }

    #[test]
    fn directory_loading_is_recursive_sorted_and_rejects_cross_file_duplicates() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(
            nested.join("z.yaml"),
            directory_scenario("zeta", "task.state-machine.legal-transitions"),
        )
        .unwrap();
        fs::write(
            directory.path().join("a.yaml"),
            directory_scenario("alpha", "task.park.reason-attribution"),
        )
        .unwrap();
        let inventory = ScenarioInventory::load(directory.path()).unwrap();
        assert_eq!(
            inventory
                .scenarios
                .iter()
                .map(|scenario| scenario.id.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );

        fs::write(
            directory.path().join("b.yaml"),
            directory_scenario("alpha", "task.state-machine.legal-transitions"),
        )
        .unwrap();
        assert!(
            ScenarioInventory::load(directory.path())
                .unwrap_err()
                .to_string()
                .contains("duplicate scenario id `alpha`")
        );
        fs::remove_file(directory.path().join("b.yaml")).unwrap();
        fs::write(
            directory.path().join("b.yaml"),
            directory_scenario("beta", "task.state-machine.legal-transitions"),
        )
        .unwrap();
        assert!(
            ScenarioInventory::load(directory.path())
                .unwrap_err()
                .to_string()
                .contains("duplicate primary coverage `task.state-machine.legal-transitions`")
        );
    }

    #[test]
    fn production_inventory_has_ten_exact_scenario_coverage_bindings() {
        let inventory = ScenarioInventory::load(root().join("qa/scenarios")).unwrap();
        inventory
            .validate(
                &Taxonomy::load(root().join("qa/taxonomy.yaml")).unwrap(),
                root(),
            )
            .unwrap();
        let bindings = inventory
            .scenarios
            .iter()
            .map(|scenario| {
                format!(
                    "{}|{}|{}",
                    scenario.id,
                    scenario.primary_coverage,
                    scenario.secondary_coverage.join(",")
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            bindings,
            vec![
                "agents.empty_turn_loop_stale_metadata|agent-loop.empty-turn-detection|agent-loop.loop-metadata-freshness",
                "db.test_template_clone_locking|test-infra.db-template-clone-lock|test-infra.deterministic-db-isolation",
                "dispatch.credential_admission_failure|dispatch.claim-admission|provider.credentials-preflight",
                "dispatch.deferred_health_terminal_audit|dispatch.deferred-health|task-lifecycle.terminal-side-effects",
                "dispatch.exhaustion_terminal_side_effects|dispatch.exhaustion-terminality|task-lifecycle.terminal-side-effects",
                "dispatch.zero_cap_slot_pool_non_vacuous|dispatch.slot-pool-capacity|dispatch.race-test-non-vacuous",
                "github.merge_queue_reopen_blindness|github.merge-queue-reopen|task-lifecycle.pr-reopen-requeue",
                "metrics.actor_metric_integration|telemetry.actor-metrics-export|agent-loop.metrics-integration",
                "reaper.stall_kill_cascade|reaper.slow-vs-crashed-discrimination|heartbeat.liveness-semantics",
                "tasks.park_reason_attribution_3t22|task-lifecycle.park-reason-attribution|task-lifecycle.park-event-causality",
            ]
        );
    }
}

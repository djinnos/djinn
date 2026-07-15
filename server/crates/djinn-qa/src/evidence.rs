use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::Path,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{Profile, Scenario, ScenarioInventory, Taxonomy};

/// The outcome recorded by a QA runner. A pass is never inferred from registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceStatus {
    Passed,
    Failed,
}

/// Identifies the runner format so a downstream runner can decide whether it can consume evidence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerIdentity {
    pub name: String,
    pub version: String,
}

/// A durable, typed record of one scenario execution.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub scenario_id: String,
    pub scenario_version: u32,
    pub taxonomy_version: u32,
    /// The requirement which the runner intended to exercise.
    pub requirement_id: String,
    pub covered_ids: Vec<String>,
    pub profile: Profile,
    pub status: EvidenceStatus,
    pub evidence_sha: String,
    /// RFC3339 timestamp emitted when execution began; used only for deterministic ordering.
    pub started_at: String,
    /// RFC3339 timestamp emitted when execution completed; used only for deterministic ordering.
    pub finished_at: String,
    pub runner: RunnerIdentity,
}

/// A versioned evidence file.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceSet {
    pub version: u32,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
}

impl EvidenceSet {
    pub fn from_yaml(yaml: &str) -> Result<Self, EvidenceError> {
        let set: Self = serde_yaml::from_str(yaml).map_err(EvidenceError::Yaml)?;
        if set.version != 1 {
            return Err(EvidenceError::UnsupportedVersion {
                version: set.version,
            });
        }
        Ok(set)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, EvidenceError> {
        let path = path.as_ref();
        let yaml = fs::read_to_string(path).map_err(|source| EvidenceError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_yaml(&yaml)
    }

    /// Validate that evidence has a complete identity against immutable taxonomy and scenario input.
    pub fn validate(
        &self,
        taxonomy: &Taxonomy,
        inventory: &ScenarioInventory,
    ) -> Result<(), EvidenceValidationErrors> {
        let scenario_by_id = inventory
            .scenarios
            .iter()
            .map(|scenario| (scenario.id.as_str(), scenario))
            .collect::<BTreeMap<_, _>>();
        let coverage = taxonomy
            .coverage
            .iter()
            .map(|entry| entry.id.as_ref())
            .collect::<BTreeSet<_>>();
        let mut errors = Vec::new();
        for record in &self.evidence {
            let prefix = format!("evidence for scenario `{}`", record.scenario_id);
            match scenario_by_id.get(record.scenario_id.as_str()) {
                None => errors.push(format!("{prefix}: scenario is not registered")),
                Some(scenario) => {
                    if record.scenario_version != scenario.version {
                        errors.push(format!(
                            "{prefix}: scenario version does not match registered scenario"
                        ));
                    }
                    let expected = covered_by(scenario);
                    if !same_ids(&record.covered_ids, &expected) {
                        errors.push(format!(
                            "{prefix}: covered ids do not match registered scenario"
                        ));
                    }
                    if !scenario.profiles.contains(&record.profile) {
                        errors.push(format!(
                            "{prefix}: profile is not eligible for registered scenario"
                        ));
                    }
                }
            }
            if record.taxonomy_version != taxonomy.version {
                errors.push(format!(
                    "{prefix}: taxonomy version does not match current taxonomy"
                ));
            }
            if !coverage.contains(record.requirement_id.as_str()) {
                errors.push(format!(
                    "{prefix}: requirement id is not present in taxonomy"
                ));
            }
            for id in &record.covered_ids {
                if !coverage.contains(id.as_str()) {
                    errors.push(format!(
                        "{prefix}: covered id `{id}` is not present in taxonomy"
                    ));
                }
            }
            if record.scenario_id.is_empty()
                || record.runner.name.is_empty()
                || record.runner.version.is_empty()
            {
                errors.push(format!(
                    "{prefix}: scenario id and runner identity must not be empty"
                ));
            }
            if !is_git_sha(&record.evidence_sha) {
                errors.push(format!(
                    "{prefix}: evidence SHA must be a 40- or 64-character hexadecimal Git object ID"
                ));
            }
            if record.scenario_version == 0 || record.taxonomy_version == 0 {
                errors.push(format!("{prefix}: versions must be greater than zero"));
            }
            let started_at = parse_rfc3339(&record.started_at);
            let finished_at = parse_rfc3339(&record.finished_at);
            if started_at.is_none()
                || finished_at.is_none()
                || started_at
                    .zip(finished_at)
                    .is_some_and(|(start, finish)| start > finish)
            {
                errors.push(format!(
                    "{prefix}: timestamps must be ordered RFC3339 values"
                ));
            }
        }
        errors.sort();
        errors.dedup();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(EvidenceValidationErrors(errors))
        }
    }
}

/// All non-filesystem classifier inputs. Callers populate this from git and source inspection.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct CoverageContext {
    pub current_sha: String,
    pub accepted_baseline_shas: BTreeSet<String>,
    /// Scenario IDs whose metadata or executable definition changed since the evidence SHA.
    pub changed_scenarios: BTreeSet<String>,
    /// Scenario IDs with a changed watched path.
    pub changed_watch_paths: BTreeSet<String>,
    /// Scenario IDs with a changed source identifier.
    pub changed_sources: BTreeSet<String>,
    /// Coverage requirement IDs changed since the evidence was recorded.
    pub changed_requirements: BTreeSet<String>,
    /// Scenario IDs which cannot currently resolve to an executable target.
    pub unresolved_executables: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CoverageState {
    Unproven,
    Proven,
    Stale,
    Failing,
}

/// One deterministic result for a coverage requirement and selected profile.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CoverageResult {
    pub coverage_id: String,
    pub profile: Profile,
    pub state: CoverageState,
    pub scenario_ids: Vec<String>,
    pub evidence_index: Option<usize>,
    pub stale_reasons: Vec<String>,
}

/// Pure coverage engine. State precedence is `failing`, then `stale`, then `proven`, then `unproven`.
pub fn classify_coverage(
    taxonomy: &Taxonomy,
    inventory: &ScenarioInventory,
    evidence: &EvidenceSet,
    profile: Profile,
    context: &CoverageContext,
) -> Vec<CoverageResult> {
    taxonomy
        .coverage
        .iter()
        .map(|entry| {
            let coverage_id = entry.id.to_string();
            let scenarios = inventory
                .scenarios
                .iter()
                .filter(|scenario| {
                    covers(scenario, &coverage_id)
                        && scenario.enabled
                        && scenario.profiles.contains(&profile)
                })
                .collect::<Vec<_>>();
            let scenario_ids = scenarios
                .iter()
                .map(|scenario| scenario.id.clone())
                .collect::<Vec<_>>();
            let mut failing = None;
            let mut stale = None;
            let mut proven = None;
            for scenario in scenarios {
                let candidate = evidence
                    .evidence
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| item.scenario_id == scenario.id)
                    // Compare normalized instants, not timestamp strings. For equal instants
                    // (and malformed evidence, which validation rejects), the higher file index
                    // wins so input ordering remains the documented deterministic tie-breaker.
                    .max_by_key(|(index, item)| (parse_rfc3339(&item.finished_at), *index));
                let Some((index, item)) = candidate else {
                    continue;
                };
                let reasons =
                    stale_reasons(item, scenario, taxonomy, &coverage_id, profile, context);
                if !reasons.is_empty() {
                    stale.get_or_insert((index, reasons));
                } else if item.status == EvidenceStatus::Failed {
                    failing.get_or_insert(index);
                } else {
                    proven.get_or_insert(index);
                }
            }
            let (state, evidence_index, stale_reasons) = if let Some(index) = failing {
                (CoverageState::Failing, Some(index), vec![])
            } else if let Some((index, reasons)) = stale {
                (CoverageState::Stale, Some(index), reasons)
            } else if let Some(index) = proven {
                (CoverageState::Proven, Some(index), vec![])
            } else {
                (CoverageState::Unproven, None, vec![])
            };
            CoverageResult {
                coverage_id,
                profile,
                state,
                scenario_ids,
                evidence_index,
                stale_reasons,
            }
        })
        .collect()
}

fn stale_reasons(
    item: &Evidence,
    scenario: &Scenario,
    taxonomy: &Taxonomy,
    coverage_id: &str,
    profile: Profile,
    context: &CoverageContext,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if item.scenario_version != scenario.version {
        reasons.push("scenario-version-mismatch".into());
    }
    if item.taxonomy_version != taxonomy.version {
        reasons.push("taxonomy-version-mismatch".into());
    }
    if item.requirement_id != coverage_id {
        reasons.push("requirement-identity-mismatch".into());
    }
    if !same_ids(&item.covered_ids, &covered_by(scenario)) {
        reasons.push("coverage-identity-mismatch".into());
    }
    if item.profile != profile {
        reasons.push("profile-mismatch".into());
    }
    if item.evidence_sha != context.current_sha
        && !context.accepted_baseline_shas.contains(&item.evidence_sha)
    {
        reasons.push("evidence-sha-not-current".into());
    }
    if context.changed_scenarios.contains(&scenario.id) {
        reasons.push("scenario-changed".into());
    }
    if context.changed_watch_paths.contains(&scenario.id) {
        reasons.push("watch-path-changed".into());
    }
    if context.changed_sources.contains(&scenario.id) {
        reasons.push("source-changed".into());
    }
    if context.changed_requirements.contains(coverage_id) {
        reasons.push("taxonomy-requirement-changed".into());
    }
    if scenario.blocked_dependency.is_some() {
        reasons.push("blocked-dependency".into());
    }
    if context.unresolved_executables.contains(&scenario.id) {
        reasons.push("executable-unresolved".into());
    }
    reasons
}

fn covered_by(scenario: &Scenario) -> Vec<String> {
    let mut ids = vec![scenario.primary_coverage.clone()];
    ids.extend(scenario.secondary_coverage.clone());
    ids.sort();
    ids.dedup();
    ids
}
fn covers(scenario: &Scenario, coverage: &str) -> bool {
    scenario.primary_coverage == coverage
        || scenario.secondary_coverage.iter().any(|id| id == coverage)
}
fn same_ids(left: &[String], right: &[String]) -> bool {
    let mut left = left.to_vec();
    left.sort();
    left.dedup();
    left == right
}
fn parse_rfc3339(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339).ok()
}

fn is_git_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Debug, Error)]
pub enum EvidenceError {
    #[error("could not read evidence `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("evidence YAML is invalid: {0}")]
    Yaml(#[source] serde_yaml::Error),
    #[error("evidence version `{version}` is unsupported; expected version `1`")]
    UnsupportedVersion { version: u32 },
}
#[derive(Debug, Eq, PartialEq)]
pub struct EvidenceValidationErrors(pub Vec<String>);
impl fmt::Display for EvidenceValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("\n"))
    }
}
impl std::error::Error for EvidenceValidationErrors {}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fn taxonomy() -> Taxonomy {
        Taxonomy::from_yaml(include_str!("../tests/fixtures/valid-taxonomy.yaml")).unwrap()
    }
    fn inventory() -> ScenarioInventory {
        ScenarioInventory::from_yaml(include_str!("../tests/fixtures/scenarios-valid.yaml"))
            .unwrap()
    }
    fn record(status: EvidenceStatus) -> Evidence {
        Evidence {
            scenario_id: "qa.cargo-test-target".into(),
            scenario_version: 1,
            taxonomy_version: 1,
            requirement_id: "reaper.slow-vs-crashed-discrimination".into(),
            covered_ids: vec!["reaper.slow-vs-crashed-discrimination".into()],
            profile: Profile::SmokeCi,
            status,
            evidence_sha: SHA.into(),
            started_at: "2026-01-01T00:00:00Z".into(),
            finished_at: "2026-01-01T00:00:01Z".into(),
            runner: RunnerIdentity {
                name: "qa".into(),
                version: "1".into(),
            },
        }
    }
    fn result(set: EvidenceSet, context: CoverageContext) -> CoverageResult {
        classify_coverage(&taxonomy(), &inventory(), &set, Profile::SmokeCi, &context)
            .into_iter()
            .find(|item| item.coverage_id.starts_with("reaper"))
            .unwrap()
    }
    #[test]
    fn classifies_all_four_states_and_never_registration_as_proof() {
        let base = CoverageContext {
            current_sha: SHA.into(),
            ..Default::default()
        };
        assert_eq!(
            result(
                EvidenceSet {
                    version: 1,
                    evidence: vec![]
                },
                base.clone()
            )
            .state,
            CoverageState::Unproven
        );
        assert_eq!(
            result(
                EvidenceSet {
                    version: 1,
                    evidence: vec![record(EvidenceStatus::Passed)]
                },
                base.clone()
            )
            .state,
            CoverageState::Proven
        );
        assert_eq!(
            result(
                EvidenceSet {
                    version: 1,
                    evidence: vec![record(EvidenceStatus::Failed)]
                },
                base
            )
            .state,
            CoverageState::Failing
        );
        let mut stale = record(EvidenceStatus::Passed);
        stale.scenario_version = 2;
        assert_eq!(
            result(
                EvidenceSet {
                    version: 1,
                    evidence: vec![stale]
                },
                CoverageContext {
                    current_sha: SHA.into(),
                    ..Default::default()
                }
            )
            .state,
            CoverageState::Stale
        );
    }
    #[test]
    fn failed_current_evidence_precedes_stale_and_reasons_are_stable() {
        let mut stale = record(EvidenceStatus::Passed);
        stale.finished_at = "2026-01-01T00:00:02Z".into();
        stale.scenario_version = 2;
        let context = CoverageContext {
            current_sha: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            ..Default::default()
        };
        let outcome = result(
            EvidenceSet {
                version: 1,
                evidence: vec![stale, record(EvidenceStatus::Failed)],
            },
            context,
        );
        assert_eq!(outcome.state, CoverageState::Stale);
        let mut stale = record(EvidenceStatus::Passed);
        stale.scenario_version = 2;
        let outcome = result(
            EvidenceSet {
                version: 1,
                evidence: vec![stale],
            },
            CoverageContext {
                current_sha: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                ..Default::default()
            },
        );
        assert_eq!(
            outcome.stale_reasons,
            vec!["scenario-version-mismatch", "evidence-sha-not-current"]
        );
    }

    #[test]
    fn rejects_invalid_rfc3339_timestamps_and_git_shas() {
        let mut invalid_timestamp = record(EvidenceStatus::Passed);
        invalid_timestamp.started_at = "0000000000TaaaaaaaaaZ".into();
        invalid_timestamp.finished_at = "0000000000TbbbbbbbbbZ".into();
        invalid_timestamp.evidence_sha = "not-a-sha".into();
        let errors = EvidenceSet {
            version: 1,
            evidence: vec![invalid_timestamp],
        }
        .validate(&taxonomy(), &inventory())
        .unwrap_err();
        assert!(
            errors
                .0
                .iter()
                .any(|error| error.contains("timestamps must be ordered RFC3339 values"))
        );
        assert!(
            errors
                .0
                .iter()
                .any(|error| error.contains("evidence SHA must be a 40- or 64-character"))
        );
    }

    #[test]
    fn selects_latest_evidence_by_normalized_rfc3339_instant() {
        let mut earlier_failure = record(EvidenceStatus::Failed);
        earlier_failure.finished_at = "2026-01-01T06:00:00+12:00".into();
        let mut later_pass = record(EvidenceStatus::Passed);
        later_pass.finished_at = "2026-01-01T00:00:00-12:00".into();
        let outcome = result(
            EvidenceSet {
                version: 1,
                evidence: vec![earlier_failure, later_pass],
            },
            CoverageContext {
                current_sha: SHA.into(),
                ..Default::default()
            },
        );
        assert_eq!(outcome.state, CoverageState::Proven);
        assert_eq!(outcome.evidence_index, Some(1));
    }
}

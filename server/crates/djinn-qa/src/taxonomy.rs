use std::{collections::BTreeSet, fmt, fs, path::Path, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A stable, dotted identifier for one behavioral coverage requirement.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CoverageId(String);

impl CoverageId {
    fn parse(value: String, index: usize) -> Result<Self, TaxonomyError> {
        if is_dotted_identifier(&value) {
            Ok(Self(value))
        } else {
            Err(TaxonomyError::MalformedCoverageId { index, value })
        }
    }
}

impl AsRef<str> for CoverageId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CoverageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The subsystem accountable for a coverage requirement.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Subsystem {
    TaskStateMachine,
    Parking,
    Breaker,
    MergeQueue,
    Dispatch,
    Provider,
    Liveness,
    Reaper,
}

impl FromStr for Subsystem {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "task-state-machine" => Ok(Self::TaskStateMachine),
            "parking" => Ok(Self::Parking),
            "breaker" => Ok(Self::Breaker),
            "merge-queue" => Ok(Self::MergeQueue),
            "dispatch" => Ok(Self::Dispatch),
            "provider" => Ok(Self::Provider),
            "liveness" => Ok(Self::Liveness),
            "reaper" => Ok(Self::Reaper),
            _ => Err(()),
        }
    }
}

/// A CI profile for which the behavior is required.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    SmokeCi,
}

impl FromStr for Profile {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "smoke-ci" => Ok(Self::SmokeCi),
            _ => Err(()),
        }
    }
}

/// One validated behavioral coverage requirement.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CoverageEntry {
    pub id: CoverageId,
    pub subsystem: Subsystem,
    pub required_profiles: Vec<Profile>,
}

/// The validated, deterministically ordered taxonomy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Taxonomy {
    pub version: u32,
    pub coverage: Vec<CoverageEntry>,
}

impl Taxonomy {
    /// Load and validate a taxonomy from a YAML document.
    pub fn from_yaml(yaml: &str) -> Result<Self, TaxonomyError> {
        let raw: RawTaxonomy = serde_yaml::from_str(yaml).map_err(TaxonomyError::Yaml)?;
        Self::validate(raw)
    }

    /// Load and validate a taxonomy from a YAML file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, TaxonomyError> {
        let path = path.as_ref();
        let yaml = fs::read_to_string(path).map_err(|source| TaxonomyError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_yaml(&yaml)
    }

    fn validate(raw: RawTaxonomy) -> Result<Self, TaxonomyError> {
        if raw.version != 1 {
            return Err(TaxonomyError::UnsupportedVersion {
                version: raw.version,
            });
        }
        if raw.coverage.is_empty() {
            return Err(TaxonomyError::EmptyCoverage);
        }

        let mut identifiers = BTreeSet::new();
        let mut coverage = Vec::with_capacity(raw.coverage.len());
        for (index, raw_entry) in raw.coverage.into_iter().enumerate() {
            let position = index + 1;
            let id = CoverageId::parse(raw_entry.id, position)?;
            if !identifiers.insert(id.clone()) {
                return Err(TaxonomyError::DuplicateCoverageId { id: id.to_string() });
            }
            let subsystem =
                raw_entry
                    .subsystem
                    .parse()
                    .map_err(|_| TaxonomyError::InvalidSubsystem {
                        id: id.to_string(),
                        value: raw_entry.subsystem,
                    })?;
            if raw_entry.required_profiles.is_empty() {
                return Err(TaxonomyError::EmptyProfiles { id: id.to_string() });
            }

            let mut profiles = BTreeSet::new();
            for profile in raw_entry.required_profiles {
                let parsed = profile.parse().map_err(|_| TaxonomyError::InvalidProfile {
                    id: id.to_string(),
                    value: profile,
                })?;
                if !profiles.insert(parsed) {
                    return Err(TaxonomyError::DuplicateProfile {
                        id: id.to_string(),
                        profile: profile_name(parsed).to_string(),
                    });
                }
            }
            coverage.push(CoverageEntry {
                id,
                subsystem,
                required_profiles: profiles.into_iter().collect(),
            });
        }
        coverage.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(Self {
            version: raw.version,
            coverage,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTaxonomy {
    version: u32,
    coverage: Vec<RawCoverageEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCoverageEntry {
    id: String,
    subsystem: String,
    required_profiles: Vec<String>,
}

/// Actionable validation failures for a taxonomy document.
#[derive(Debug, Error)]
pub enum TaxonomyError {
    #[error("could not read taxonomy `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("taxonomy YAML is invalid: {0}")]
    Yaml(#[source] serde_yaml::Error),
    #[error("taxonomy version `{version}` is unsupported; expected version `1`")]
    UnsupportedVersion { version: u32 },
    #[error("taxonomy must declare at least one coverage entry")]
    EmptyCoverage,
    #[error(
        "coverage entry {index} has malformed id `{value}`; use lowercase dotted segments such as `task.state-machine.legal-transitions`"
    )]
    MalformedCoverageId { index: usize, value: String },
    #[error("duplicate coverage id `{id}`; every coverage id must be unique")]
    DuplicateCoverageId { id: String },
    #[error(
        "coverage id `{id}` has invalid subsystem `{value}`; expected one of task-state-machine, parking, breaker, merge-queue, dispatch, provider, liveness, reaper"
    )]
    InvalidSubsystem { id: String, value: String },
    #[error("coverage id `{id}` must require at least one profile")]
    EmptyProfiles { id: String },
    #[error("coverage id `{id}` has invalid profile `{value}`; expected `smoke-ci`")]
    InvalidProfile { id: String, value: String },
    #[error("coverage id `{id}` declares profile `{profile}` more than once")]
    DuplicateProfile { id: String, profile: String },
}

fn is_dotted_identifier(value: &str) -> bool {
    value.split('.').count() >= 2
        && value.split('.').all(|segment| {
            !segment.is_empty()
                && segment.bytes().enumerate().all(|(index, byte)| match byte {
                    b'a'..=b'z' => true,
                    b'0'..=b'9' | b'-' => index > 0,
                    _ => false,
                })
        })
}

fn profile_name(profile: Profile) -> &'static str {
    match profile {
        Profile::SmokeCi => "smoke-ci",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> &'static str {
        match name {
            "valid-taxonomy" => include_str!("../tests/fixtures/valid-taxonomy.yaml"),
            "duplicate-id" => include_str!("../tests/fixtures/duplicate-id.yaml"),
            "malformed-id" => include_str!("../tests/fixtures/malformed-id.yaml"),
            "invalid-subsystem" => include_str!("../tests/fixtures/invalid-subsystem.yaml"),
            "empty-profiles" => include_str!("../tests/fixtures/empty-profiles.yaml"),
            "invalid-profile" => include_str!("../tests/fixtures/invalid-profile.yaml"),
            _ => unreachable!("unknown fixture"),
        }
    }

    #[test]
    fn loads_valid_taxonomy_in_identifier_order() {
        let taxonomy = Taxonomy::from_yaml(fixture("valid-taxonomy")).expect("valid taxonomy");

        assert_eq!(taxonomy.coverage.len(), 2);
        assert_eq!(
            taxonomy.coverage[0].id.as_ref(),
            "reaper.slow-vs-crashed-discrimination"
        );
        assert_eq!(
            taxonomy.coverage[1].id.as_ref(),
            "task.state-machine.legal-transitions"
        );
        assert_eq!(
            taxonomy.coverage[0].required_profiles,
            vec![Profile::SmokeCi]
        );
    }

    #[test]
    fn rejects_duplicate_coverage_ids() {
        let error =
            Taxonomy::from_yaml(fixture("duplicate-id")).expect_err("duplicate id rejected");
        assert_eq!(
            error.to_string(),
            "duplicate coverage id `task.state-machine.legal-transitions`; every coverage id must be unique"
        );
    }

    #[test]
    fn rejects_malformed_coverage_ids() {
        let error =
            Taxonomy::from_yaml(fixture("malformed-id")).expect_err("malformed id rejected");
        assert!(
            error
                .to_string()
                .contains("malformed id `Task.state-machine`")
        );
    }

    #[test]
    fn rejects_invalid_subsystem_ownership() {
        let error = Taxonomy::from_yaml(fixture("invalid-subsystem"))
            .expect_err("invalid subsystem rejected");
        assert!(
            error
                .to_string()
                .contains("invalid subsystem `unknown-system`")
        );
    }

    #[test]
    fn rejects_missing_subsystem_ownership() {
        let error = Taxonomy::from_yaml(
            "version: 1\ncoverage:\n  - id: task.state-machine.legal-transitions\n    required_profiles: [smoke-ci]\n",
        )
        .expect_err("missing subsystem rejected");
        assert!(error.to_string().contains("missing field `subsystem`"));
    }

    #[test]
    fn rejects_empty_profile_requirements() {
        let error =
            Taxonomy::from_yaml(fixture("empty-profiles")).expect_err("empty profiles rejected");
        assert_eq!(
            error.to_string(),
            "coverage id `task.state-machine.legal-transitions` must require at least one profile"
        );
    }

    #[test]
    fn rejects_invalid_profile_requirements() {
        let error =
            Taxonomy::from_yaml(fixture("invalid-profile")).expect_err("invalid profile rejected");
        assert!(error.to_string().contains("invalid profile `production`"));
    }
}

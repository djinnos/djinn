use std::{collections::BTreeMap, path::Path};

use serde::Serialize;
use thiserror::Error;

use crate::{
    CoverageContext, CoverageState, EvidenceSet, Profile, ScenarioInventory, Taxonomy,
    classify_coverage,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CoverageReportRow {
    pub coverage_id: String,
    pub subsystem: String,
    pub required_profiles: Vec<String>,
    pub state: CoverageState,
    pub scenario_ids: Vec<String>,
    pub evidence_path: Option<String>,
    pub last_passed_at: Option<String>,
    pub last_evidence_sha: Option<String>,
    pub stale_reasons: Vec<String>,
    pub memory_sources: Vec<String>,
}
fn subsystem_name(subsystem: crate::Subsystem) -> &'static str {
    match subsystem {
        crate::Subsystem::TaskStateMachine => "task-state-machine",
        crate::Subsystem::Parking => "parking",
        crate::Subsystem::Breaker => "breaker",
        crate::Subsystem::MergeQueue => "merge-queue",
        crate::Subsystem::Dispatch => "dispatch",
        crate::Subsystem::Provider => "provider",
        crate::Subsystem::Liveness => "liveness",
        crate::Subsystem::Reaper => "reaper",
    }
}

pub fn coverage_report(
    taxonomy: &Taxonomy,
    inventory: &ScenarioInventory,
    evidence: &EvidenceSet,
    profile: Profile,
    context: &CoverageContext,
    evidence_path: Option<&Path>,
) -> Vec<CoverageReportRow> {
    let entries = taxonomy
        .coverage
        .iter()
        .map(|entry| (entry.id.to_string(), entry))
        .collect::<BTreeMap<_, _>>();
    classify_coverage(taxonomy, inventory, evidence, profile, context)
        .into_iter()
        .map(|result| {
            let entry = entries[&result.coverage_id];
            let mut memory_sources = inventory
                .scenarios
                .iter()
                .filter(|scenario| result.scenario_ids.binary_search(&scenario.id).is_ok())
                .flat_map(|scenario| {
                    scenario
                        .sources
                        .iter()
                        .map(|source| format!("{:?}:{}", source.kind, source.id))
                })
                .collect::<Vec<_>>();
            memory_sources.sort();
            memory_sources.dedup();
            let record = result
                .evidence_index
                .and_then(|index| evidence.evidence.get(index));
            CoverageReportRow {
                coverage_id: result.coverage_id,
                subsystem: subsystem_name(entry.subsystem).to_owned(),
                required_profiles: entry
                    .required_profiles
                    .iter()
                    .map(|profile| profile_name(*profile).to_owned())
                    .collect(),
                state: result.state,
                scenario_ids: result.scenario_ids,
                evidence_path: evidence_path.map(|path| path.display().to_string()),
                last_passed_at: record
                    .filter(|item| matches!(item.status, crate::EvidenceStatus::Passed))
                    .map(|item| item.finished_at.clone()),
                last_evidence_sha: record.map(|item| item.evidence_sha.clone()),
                stale_reasons: result.stale_reasons,
                memory_sources,
            }
        })
        .collect()
}
pub fn required_gap(rows: &[CoverageReportRow], profile: Profile) -> bool {
    rows.iter().any(|row| {
        row.required_profiles
            .binary_search(&profile_name(profile).to_owned())
            .is_ok()
            && row.state != CoverageState::Proven
    })
}
fn profile_name(profile: Profile) -> &'static str {
    match profile {
        Profile::SmokeCi => "smoke-ci",
    }
}
#[derive(Debug, Error)]
pub enum ReportError {
    #[error("repository root `{0}` does not contain qa/taxonomy.yaml")]
    MissingTaxonomy(String),
    #[error("repository root `{0}` is not a directory")]
    InvalidRoot(String),
}
pub fn discovered_root(start: &Path) -> Result<std::path::PathBuf, ReportError> {
    let start = start
        .canonicalize()
        .map_err(|_| ReportError::InvalidRoot(start.display().to_string()))?;
    start
        .ancestors()
        .find(|path| path.join("qa/taxonomy.yaml").is_file())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ReportError::MissingTaxonomy(start.display().to_string()))
}
pub fn empty_inventory() -> ScenarioInventory {
    ScenarioInventory {
        version: 1,
        scenarios: vec![],
    }
}
pub fn empty_evidence() -> EvidenceSet {
    EvidenceSet {
        version: 1,
        evidence: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_inventory_honestly_reports_unproven_rows() {
        let taxonomy = Taxonomy::from_yaml("version: 1\ncoverage:\n- id: task.state-machine.legal-transitions\n  subsystem: task-state-machine\n  required_profiles: [smoke-ci]\n").unwrap();
        let rows = coverage_report(
            &taxonomy,
            &empty_inventory(),
            &empty_evidence(),
            Profile::SmokeCi,
            &CoverageContext::default(),
            None,
        );
        assert_eq!(rows[0].state, CoverageState::Unproven);
        assert!(required_gap(&rows, Profile::SmokeCi));
        assert_eq!(
            serde_json::to_value(&rows[0])
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            10
        );
    }
}

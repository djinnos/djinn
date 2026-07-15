//! Repository-local behavioral QA contracts.
//!
//! Taxonomy and scenario inventory validation for repository-local behavioral QA.

pub mod evidence;
pub mod report;
pub mod scenario;
pub mod taxonomy;

pub use evidence::{
    CoverageContext, CoverageResult, CoverageState, Evidence, EvidenceError, EvidenceSet,
    EvidenceStatus, EvidenceValidationErrors, RunnerIdentity, classify_coverage,
};
pub use report::{
    CoverageReportRow, ReportError, coverage_report, discovered_root, empty_evidence,
    empty_inventory, required_gap,
};
pub use scenario::{
    Execution, Isolation, IsolationMode, Scenario, ScenarioError, ScenarioInventory,
    ScenarioValidationErrors, SourceIdentifier, SourceKind,
};
pub use taxonomy::{CoverageEntry, CoverageId, Profile, Subsystem, Taxonomy, TaxonomyError};

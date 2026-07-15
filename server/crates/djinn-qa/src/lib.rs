//! Repository-local behavioral QA contracts.
//!
//! Taxonomy and scenario inventory validation for repository-local behavioral QA.

pub mod scenario;
pub mod taxonomy;

pub use scenario::{
    Execution, Isolation, IsolationMode, Scenario, ScenarioError, ScenarioInventory,
    ScenarioValidationErrors, SourceIdentifier, SourceKind,
};
pub use taxonomy::{CoverageEntry, CoverageId, Profile, Subsystem, Taxonomy, TaxonomyError};

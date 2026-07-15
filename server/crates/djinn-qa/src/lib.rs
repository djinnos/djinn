//! Repository-local behavioral QA contracts.
//!
<<<<<<< HEAD
//! Taxonomy and scenario inventory validation for repository-local behavioral QA.

pub mod scenario;
pub mod taxonomy;

pub use scenario::{
    Execution, Isolation, IsolationMode, Scenario, ScenarioError, ScenarioInventory,
    ScenarioValidationErrors, SourceIdentifier, SourceKind,
};
=======
//! This crate deliberately owns only taxonomy loading in its initial slice.
//! Scenario execution and coverage reporting are layered on later.

pub mod taxonomy;

>>>>>>> origin/main
pub use taxonomy::{CoverageEntry, CoverageId, Profile, Subsystem, Taxonomy, TaxonomyError};

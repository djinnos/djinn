//! Repository-local behavioral QA contracts.
//!
//! This crate deliberately owns only taxonomy loading in its initial slice.
//! Scenario execution and coverage reporting are layered on later.

pub mod taxonomy;

pub use taxonomy::{CoverageEntry, CoverageId, Profile, Subsystem, Taxonomy, TaxonomyError};

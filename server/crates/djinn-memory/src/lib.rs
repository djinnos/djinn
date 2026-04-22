//! Knowledge-base / memory types extracted from `djinn-core`.
//!
//! This crate owns the data structures for notes, the wikilink knowledge graph,
//! the health report, contradiction candidates, and the ADR-054 extracted-note
//! audit. It also publishes event-envelope constructors for note lifecycle
//! events so that `djinn-db` and `djinn-control-plane` do not need to reach into
//! `djinn-core::events` for note-shaped payloads.
//!
//! Types in this crate are deliberately dependency-light: they only pull in
//! `serde`, `schemars`, and (optionally, behind the `sqlx` feature) `sqlx`
//! for `FromRow` derives. The `events` module re-uses
//! `djinn_core::events::DjinnEventEnvelope`.

pub mod consolidation;
pub mod events;
pub mod note;
pub mod note_association;

pub use consolidation::{
    ConsolidatedNoteProvenance, ConsolidationCandidateEdge, ConsolidationCluster,
    ConsolidationNote, ConsolidationRunMetric, DbNoteGroup,
};
pub use note::{
    BrokenLink, BrokenLinkRepair, BuildContextResponse, ContradictionCandidate,
    ExtractedNoteAuditCategory, ExtractedNoteAuditFinding, ExtractedNoteAuditReport, GitLogEntry,
    GraphEdge, GraphNode, GraphResponse, HealthReport, Note, NoteAbstract, NoteCompact,
    NoteDedupCandidate, NoteOverview, NoteSearchResult, OrphanNote, ReindexSummary, StaleFolder,
    TypeRisk,
};
pub use note_association::{NoteAssociation, canonical_pair};

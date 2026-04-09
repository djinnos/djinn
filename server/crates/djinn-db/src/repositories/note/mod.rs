use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use sqlx::Sqlite;

use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::{
    BrokenLink, GraphEdge, GraphNode, GraphResponse, HealthReport, Note, NoteCompact,
    NoteSearchResult, OrphanNote, ReindexSummary, StaleFolder,
};

use crate::database::Database;
use crate::error::{DbError as Error, DbResult as Result};

mod association;
pub(crate) mod consolidation;
mod context;
mod crud;
mod file_helpers;
mod graph;
mod housekeeping;
mod indexing;
pub(crate) mod rrf;
mod scoring;
mod search;

pub use association::NoteAssociationEntry;
pub use consolidation::{
    CreateCanonicalConsolidatedNote, CreateConsolidationRunMetric,
    CreatedCanonicalConsolidatedNote, NoteConsolidationRepository,
};
pub use context::BuildContextResponse;
pub use djinn_core::models::{
    ConsolidatedNoteProvenance, ConsolidationCandidateEdge, ConsolidationCluster,
    ConsolidationNote, ConsolidationRunMetric, ContradictionCandidate, DbNoteGroup,
    NoteDedupCandidate,
};
pub use scoring::{
    CO_ACCESS_HIGH, CONFIDENCE_CEILING, CONFIDENCE_FLOOR, CONTRADICTION, STALE_CITATION,
    USER_CONFIRM, bayesian_update,
};

pub use indexing::UpdateNoteIndexParams;

use file_helpers::{build_catalog, write_note_file};
pub use file_helpers::{
    file_path_for, file_path_for_with_status, folder_for_type, folder_for_type_with_status,
    is_singleton, permalink_for, permalink_for_with_status, slugify,
};
use indexing::{index_links_for_note, resolve_links_for_note};

// ── SQL constant ─────────────────────────────────────────────────────────────

const NOTE_SELECT_WHERE_ID: &str = "SELECT id, project_id, permalink, title, file_path,
            storage, note_type, folder, tags, content,
            created_at, updated_at, last_accessed,
            access_count, confidence, abstract as abstract_, overview,
            scope_paths
     FROM notes WHERE id = ?1";

// ── Repository ────────────────────────────────────────────────────────────────

pub struct NoteRepository {
    db: Database,
    events: EventBus,
    worktree_root: Option<PathBuf>,
}

impl NoteRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self {
            db,
            events,
            worktree_root: None,
        }
    }

    pub fn with_worktree_root(mut self, worktree_root: Option<PathBuf>) -> Self {
        self.worktree_root = worktree_root;
        self
    }

    pub fn worktree_root(&self) -> Option<&Path> {
        self.worktree_root.as_deref()
    }
}

#[cfg(test)]
mod tests;

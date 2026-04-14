use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use sqlx::Sqlite;

use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::{
    BrokenLink, GraphEdge, GraphNode, GraphResponse, HealthReport, Note, NoteCompact,
    NoteSearchResult, OrphanNote, ReindexSummary, StaleFolder,
};
use std::sync::Arc;

use crate::database::Database;
use crate::error::{DbError as Error, DbResult as Result};

mod association;
pub(crate) mod consolidation;
mod context;
mod crud;
mod embeddings;
mod file_helpers;
mod graph;
mod housekeeping;
mod indexing;
mod lexical_search;
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
pub use embeddings::{
    EmbeddedNote, NoopNoteVectorStore, NoteEmbeddingMatch, NoteEmbeddingProvider,
    NoteEmbeddingRecord, NoteVectorBackend, NoteVectorStore, QdrantNoteVectorStore,
    SqliteVecNoteVectorStore, UpsertNoteEmbedding,
};
pub use lexical_search::{
    LexicalSearchBackend, LexicalSearchMode, LexicalSearchPlan, build_lexical_search_plan,
    sanitize_mysql_boolean_query, sanitize_sqlite_fts5_query, validate_mysql_fulltext_threshold,
};
pub use scoring::{
    CO_ACCESS_HIGH, CONFIDENCE_CEILING, CONFIDENCE_FLOOR, CONTRADICTION, STALE_CITATION,
    USER_CONFIRM, bayesian_update,
};

pub use indexing::UpdateNoteIndexParams;

use file_helpers::{build_catalog, write_note_file};
pub use file_helpers::{
    file_path_for, file_path_for_with_status, folder_for_type, folder_for_type_with_status,
    infer_note_type, is_singleton, normalize_virtual_note_path, permalink_for,
    permalink_for_with_status, permalink_from_virtual_note_path, render_note_markdown, slugify,
    title_from_permalink, virtual_note_path_for_permalink,
};
use indexing::{index_links_for_note, resolve_links_for_note};

#[derive(Debug, Clone)]
pub struct NoteSearchParams<'a> {
    pub project_id: &'a str,
    pub query: &'a str,
    pub task_id: Option<&'a str>,
    pub folder: Option<&'a str>,
    pub note_type: Option<&'a str>,
    pub limit: usize,
    pub semantic_scores: Option<Vec<(String, f64)>>,
}

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
    embedding_provider: Option<Arc<dyn NoteEmbeddingProvider>>,
    vector_store: Arc<dyn NoteVectorStore>,
}

impl NoteRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self {
            db,
            events,
            worktree_root: None,
            embedding_provider: None,
            vector_store: Arc::new(SqliteVecNoteVectorStore) as Arc<dyn NoteVectorStore>,
        }
    }

    pub fn with_worktree_root(mut self, worktree_root: Option<PathBuf>) -> Self {
        self.worktree_root = worktree_root;
        self
    }

    pub fn with_embedding_provider(
        mut self,
        embedding_provider: Option<Arc<dyn NoteEmbeddingProvider>>,
    ) -> Self {
        self.embedding_provider = embedding_provider;
        self
    }

    pub fn with_vector_store(mut self, vector_store: Option<Arc<dyn NoteVectorStore>>) -> Self {
        if let Some(vector_store) = vector_store {
            self.vector_store = vector_store;
        }
        self
    }

    pub fn worktree_root(&self) -> Option<&Path> {
        self.worktree_root.as_deref()
    }

    pub fn embedding_provider(&self) -> Option<Arc<dyn NoteEmbeddingProvider>> {
        self.embedding_provider.clone()
    }

    pub fn vector_store(&self) -> Arc<dyn NoteVectorStore> {
        self.vector_store.clone()
    }
}

#[cfg(test)]
mod tests;

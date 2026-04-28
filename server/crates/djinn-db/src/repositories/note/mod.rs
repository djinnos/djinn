use std::collections::{HashMap, HashSet};

use djinn_core::events::EventBus;
use djinn_memory::{
    BrokenLink, ExtractedNoteAuditReport, GraphEdge, GraphNode, GraphResponse, HealthReport, Note,
    NoteCompact, NoteSearchResult, OrphanNote, StaleFolder,
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
pub mod rrf;
mod scoring;
mod search;

// Note: as of the db-only knowledge-base cut-over, `indexing` exposes only
// the wikilink graph helpers (used by `crud.rs`). The on-disk reindex
// pipeline (`reindex_from_disk`, `scan_project_notes`, `ScannedNote`,
// `UpdateNoteIndexParams`, `ReindexSummary`, …) was deleted because notes
// are no longer mirrored to disk.

pub use association::NoteAssociationEntry;
pub use consolidation::{
    CreateCanonicalConsolidatedNote, CreateConsolidationRunMetric,
    CreatedCanonicalConsolidatedNote, NoteConsolidationRepository,
};
pub use djinn_memory::{
    BuildContextResponse, ConsolidatedNoteProvenance, ConsolidationCandidateEdge,
    ConsolidationCluster, ConsolidationNote, ConsolidationRunMetric, ContradictionCandidate,
    DbNoteGroup, NoteDedupCandidate,
};
pub use embeddings::{
    EmbeddedNote, EmbeddingQueryContext, NoopNoteVectorStore, NoteEmbeddingMatch,
    NoteEmbeddingProvider, NoteEmbeddingRecord, NoteRepairEmbeddingRow, NoteVectorBackend,
    NoteVectorStore, QdrantConfig, QdrantNoteVectorStore, UpsertNoteEmbedding,
    embedding_content_hash, embedding_document_text, infer_embedding_branch_from_worktree,
    task_branch_name,
};
pub use lexical_search::{
    LexicalSearchBackend, LexicalSearchMode, LexicalSearchPlan, build_lexical_search_plan,
    executable_lexical_search_sql, lexical_search_threshold, normalize_lexical_score,
    sanitize_mysql_boolean_query, sanitize_sqlite_fts5_query, validate_mysql_fulltext_threshold,
};
pub use rrf::rrf_fuse;
pub use scoring::{
    CO_ACCESS_HIGH, CONFIDENCE_CEILING, CONFIDENCE_FLOOR, CONTRADICTION, STALE_CITATION,
    USER_CONFIRM, bayesian_update,
};

use file_helpers::build_catalog;
pub use file_helpers::{
    folder_for_type, folder_for_type_with_status, infer_note_type, is_singleton,
    normalize_virtual_note_path, permalink_for, permalink_for_with_status,
    permalink_from_virtual_note_path, render_note_markdown, slugify, title_from_permalink,
    virtual_note_path_for_permalink,
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

/// Expands to a `sqlx::query_as!(Note, "...", $id)` call with the full
/// SELECT projection for a `Note` row keyed by id.
///
/// Defined as a `macro_rules!` rather than a `const &str` because
/// `sqlx::query_as!` requires its SQL to be a string-literal token;
/// it does not accept a macro expansion (not even through `concat!`).
/// Call sites use `note_select_where_id!($id)` (takes the id expr).
macro_rules! note_select_where_id {
    ($id:expr) => {
        ::sqlx::query_as!(
            ::djinn_memory::Note,
            r#"SELECT id, project_id, permalink, title, file_path,
                storage, note_type, folder, tags, content,
                created_at, updated_at, last_accessed,
                access_count, confidence, `abstract` as abstract_, overview,
                scope_paths
             FROM notes WHERE id = ?"#,
            $id
        )
    };
}
pub(super) use note_select_where_id;

// ── Repository ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct NoteRepository {
    db: Database,
    events: EventBus,
    embedding_provider: Option<Arc<dyn NoteEmbeddingProvider>>,
    embedding_branch: String,
    vector_store: Arc<dyn NoteVectorStore>,
}

impl NoteRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self {
            db,
            events,
            embedding_provider: None,
            embedding_branch: "main".to_string(),
            vector_store: Arc::new(NoopNoteVectorStore) as Arc<dyn NoteVectorStore>,
        }
    }

    pub fn with_embedding_provider(
        mut self,
        embedding_provider: Option<Arc<dyn NoteEmbeddingProvider>>,
    ) -> Self {
        self.embedding_provider = embedding_provider;
        self
    }

    pub fn with_embedding_branch(mut self, embedding_branch: Option<String>) -> Self {
        if let Some(embedding_branch) = embedding_branch {
            self.embedding_branch = embedding_branch;
        }
        self
    }

    pub fn with_vector_store(mut self, vector_store: Option<Arc<dyn NoteVectorStore>>) -> Self {
        if let Some(vector_store) = vector_store {
            self.vector_store = vector_store;
        }
        self
    }

    pub fn embedding_provider(&self) -> Option<Arc<dyn NoteEmbeddingProvider>> {
        self.embedding_provider.clone()
    }

    pub fn embedding_branch(&self) -> &str {
        &self.embedding_branch
    }

    pub fn vector_store(&self) -> Arc<dyn NoteVectorStore> {
        self.vector_store.clone()
    }
}

#[cfg(test)]
mod tests;

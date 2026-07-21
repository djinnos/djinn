use std::collections::{HashMap, HashSet};
use std::time::Duration;

use djinn_core::events::EventBus;
use djinn_memory::{
    BrokenLink, ExtractedNoteAuditReport, GraphEdge, GraphLifecycleSummary, GraphNode,
    GraphOptions, GraphResponse, HealthReport, Note, NoteCompact, NoteSearchResult, OrphanNote,
    StaleFolder, TypedEdge,
};
use std::sync::Arc;

use crate::database::Database;
use crate::error::{DbError as Error, DbResult as Result};

mod association;
pub(crate) mod consolidation;
mod context;
mod crud;
mod embedding_associations;
mod embeddings;

pub use embedding_associations::EmbeddingAssociationRefreshStats;
mod entity_association;
mod file_helpers;
mod graph;
mod guidance;
mod housekeeping;
mod indexing;
mod lexical_search;
pub(crate) mod lifecycle;
mod mutation;
mod note_quality;
pub mod replay_validation;
mod revisions;
pub mod rrf;
mod scoring;
mod search;

// Note: as of the db-only knowledge-base cut-over, `indexing` exposes only
// the wikilink graph helpers (used by `crud.rs`). The on-disk reindex
// pipeline (`reindex_from_disk`, `scan_project_notes`, `ScannedNote`,
// `UpdateNoteIndexParams`, `ReindexSummary`, …) was deleted because notes
// are no longer mirrored to disk.

pub use association::{
    NoteAssociationEntry, NoteAssociationKind, NoteAssociationProvenanceRow,
    NoteAssociationProvenanceUpsert, NoteAssociationSource,
};
pub use consolidation::{
    CreateCanonicalConsolidatedNote, CreateConsolidationRunMetric,
    CreatedCanonicalConsolidatedNote, NoteConsolidationRepository,
};
pub use djinn_memory::{
    BuildContextResponse, ConsolidatedNoteProvenance, ConsolidationCandidateEdge,
    ConsolidationCluster, ConsolidationNote, ConsolidationRunMetric, ContradictionCandidate,
    DbNoteGroup, NoteDedupCandidate, NoteQualityAssessment,
};
pub use embeddings::{
    EligibleEmbeddingNote, EmbeddedNote, EmbeddingCandidate, EmbeddingQueryContext,
    NoopNoteVectorStore, NoteEmbeddingMatch, NoteEmbeddingProvider, NoteEmbeddingRecord,
    NoteRepairEmbeddingRow, NoteVectorBackend, NoteVectorStore, QdrantConfig,
    QdrantNoteVectorStore, UpsertNoteEmbedding, embedding_content_hash, embedding_document_text,
    infer_embedding_branch_from_worktree, legacy_embedding_document_text, task_branch_name,
};
pub use entity_association::{
    MemoryEntityAssociation, MemoryEntityKind, MemoryEntityRef, MemoryEntityType,
};
pub use lexical_search::{
    LexicalSearchBackend, LexicalSearchMode, LexicalSearchPlan, build_lexical_search_plan,
    executable_lexical_search_sql, lexical_search_threshold, normalize_lexical_score,
    sanitize_postgres_tsquery, sanitize_sqlite_fts5_query, validate_postgres_tsvector_threshold,
};
pub use note_quality::{assess_note_quality, looks_task_local, required_sections};
pub use replay_validation::{
    PromptBudgetReport, QueryReplayReport, RankedHit, RankingReport, ReplayCriteria, ReplayFixture,
    ReplayNote, ReplayQuery, ReplayReport, anchor_embedding_replay_fixture,
    generate_anchor_embedding_replay_report, render_anchor_embedding_replay_report_markdown,
};
pub use revisions::{
    NoteHistoryRequest, NoteRevisionActorKind, NoteRevisionEventInput, NoteRevisionEventKind,
    NoteRevisionEventRow, NoteRevisionReason, NoteRevisionSnapshot, NoteRevisionSubsystem,
    NoteRevisionValidationError, REVISION_PAGE_MAX, RevisionCursor, RevisionCursorError,
    RevisionHistoryPage, RevisionLookupRequest, RevisionRangeRequest, SessionRevisionPage,
    SessionRevisionRequest, TrustedNoteRevisionAttribution, TrustedNoteRevisionProvenance,
};
pub use rrf::rrf_fuse;
pub use scoring::{
    CO_ACCESS_HIGH, CONFIDENCE_CEILING, CONFIDENCE_FLOOR, CONTRADICTION, STALE_CITATION,
    STALE_DECAY_SIGNAL, USER_CONFIRM, bayesian_update, decay_signal_for_elapsed_days,
};

use file_helpers::build_catalog;
pub use file_helpers::{
    folder_for_type, folder_for_type_with_status, infer_note_type, is_singleton,
    normalize_virtual_note_path, permalink_for, permalink_for_with_status,
    permalink_from_virtual_note_path, render_note_markdown, slugify, title_from_permalink,
    virtual_note_path_for_permalink,
};
use indexing::{index_links_for_note, resolve_links_for_note};

pub use guidance::{
    FileEraGuidanceClassification, FileEraGuidanceDiscovery, FileEraGuidanceManifest,
    FileEraGuidanceManifestRecord,
};
pub use housekeeping::{
    AnchorProposerKind, BackfillRetrievalAnchorOptions, BackfillRetrievalAnchorReport,
    LlmAnchorProposer, ProposedBackfillAnchor, propose_anchor_deterministic,
};
pub use lifecycle::NoteStatus;
pub use mutation::{
    NoteRevisionCreateState, NoteRevisionDesiredState, NoteRevisionEvent, NoteRevisionMutation,
    NoteRevisionMutationResult, NoteRevisionUpdateState, NoteSupersedesAssociation,
};

/// Compact scope-overlap candidate row returned by
/// [`NoteRepository::query_by_scope_overlap_trace_candidates`].
///
/// This intentionally carries ranking and scope/source metadata but omits note
/// content so downstream trace classification can record why eligible notes
/// were injected or skipped without changing production injection output.
///
/// Unlike the production [`NoteRepository::query_by_scope_overlap`] query,
/// the trace-candidate query omits the confidence threshold and production
/// injection limit. This means the result set includes below-threshold active
/// notes (classifiable as `min_confidence`) and over-production-limit active
/// notes (classifiable as `not_top_k`) for downstream `mwtv` instrumentation.
///
/// Each row maps 1:1 to a [`TraceCandidate`](crate::repositories::retrieval_trace::TraceCandidate)
/// for JSONB persistence in `retrieval_traces`. The identity fields (`id`,
/// `permalink`, `title`) and provenance fields (`folder`, `note_type`,
/// `scope_paths`) are consumed by `liso` (`memory_recall_trace` tooling).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct ScopeOverlapTraceCandidate {
    pub id: String,
    pub permalink: String,
    pub title: String,
    pub folder: String,
    pub note_type: String,
    pub scope_paths: String,
    pub confidence: f64,
    pub rank: i64,
}

#[derive(Debug, Clone)]
pub struct NoteSearchParams<'a> {
    pub project_id: &'a str,
    pub query: &'a str,
    pub task_id: Option<&'a str>,
    pub folder: Option<&'a str>,
    pub note_type: Option<&'a str>,
    pub limit: usize,
    pub semantic_scores: Option<Vec<(String, f64)>>,
    /// Optional list of edge kinds to include in graph proximity scoring.
    ///
    /// When `Some`, only edges whose `kind` matches participate in spreading
    /// activation. `None` means all kinds (including `embedding_related`).
    ///
    /// Known edge kinds:
    /// - `co_access` — Hebbian co-access (symmetric, `HOP_DECAY * weight`).
    /// - `derived_from` — provenance (`HOP_DECAY * weight`).
    /// - `builds_on` — dependency (`HOP_DECAY * 0.8 * weight`).
    /// - `exemplifies` — example link (`HOP_DECAY * 0.7 * weight`).
    /// - `embedding_related` — **machine-minted** embedding similarity edges.
    ///   Lower/medium strength (`HOP_DECAY * 0.5 * weight`); provenance-
    ///   filterable via this parameter.  Included by default when no filter
    ///   is provided, but never exceeds wikilink or authored/co-access edges.
    /// - `authored` — manually curated edge.
    /// - `contradicts` — generates a `ContradictionWarning` but no score.
    /// - `supersedes` — asymmetric demotion/boost.
    pub edge_kinds: Option<&'a [String]>,
    /// Optional entity-type filter for unified search.
    ///
    /// * `None` — return both notes and proposals (the default for every
    ///   existing call site that does not set this field).
    /// * `Some(["note"])` — notes-only.
    /// * `Some(["proposal"])` — proposals-only.
    /// * `Some([])` (empty slice) — treated as "no entities"; returns an
    ///   empty result immediately.
    /// * `Some(["unknown"])` or any value not matching `"note"` or
    ///   `"proposal"` — treated as "no matching entities"; returns an
    ///   empty result.
    pub entity_types: Option<&'a [String]>,
}

/// Bounded aggregate facts about a timed note search.
///
/// This deliberately stores counts rather than candidate rows or score samples,
/// so collecting search timings cannot retain unbounded query data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoteSearchSummary {
    /// Number of unique note candidates entering the note-side ranking pipeline.
    pub candidate_count: usize,
    /// Number of unified note/proposal rows returned after applying the limit.
    pub result_count: usize,
}

/// Result of [`NoteRepository::search_with_stats`].
///
/// A duration is present only when that stage actually ran. In particular,
/// semantic scoring is absent when no semantic candidates were supplied, and
/// temporal, graph, and RRF fusion are absent when lexical/semantic candidate
/// generation produced no note candidates.
#[derive(Debug, Clone)]
pub struct TimedNoteSearchResult {
    pub rows: Vec<djinn_memory::MemorySearchEntityRow>,
    pub lexical_duration: Option<Duration>,
    pub semantic_duration: Option<Duration>,
    pub temporal_duration: Option<Duration>,
    pub graph_duration: Option<Duration>,
    pub rrf_fuse_duration: Option<Duration>,
    pub summary: NoteSearchSummary,
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
        ::sqlx::query_as::<_, ::djinn_memory::Note>(
            r#"SELECT id, project_id, permalink, title, file_path,
                storage, note_type, folder, status, tags::text AS tags, content,
                retrieval_anchor, created_at, updated_at, lifecycle_changed_at, last_accessed,
                access_count, confidence, abstract as abstract_, overview,
                scope_paths::text AS scope_paths
             FROM notes WHERE id = $1"#,
        )
        .bind($id)
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
    revision_event_failure: std::sync::Arc<std::sync::atomic::AtomicBool>,
    association_failure: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl NoteRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self {
            db,
            events,
            embedding_provider: None,
            embedding_branch: "main".to_string(),
            vector_store: Arc::new(NoopNoteVectorStore) as Arc<dyn NoteVectorStore>,
            revision_event_failure: mutation::revision_failure_flag(),
            association_failure: mutation::revision_failure_flag(),
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

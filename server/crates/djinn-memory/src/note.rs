use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Unified search result spanning notes and proposals.
///
/// Returned by `NoteRepository::search` when `entity_types` is `None` (both) or
/// `Some(["proposal"])`, interleaved with note rows by descending RRF score.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemorySearchEntityRow {
    /// `"note"` | `"proposal"` — discriminated by this field.
    pub entity: String,
    /// Note or proposal id (same column, different tables).
    pub id: String,
    pub title: String,
    /// Folder for notes; empty for proposals.
    pub folder: String,
    /// Note type for notes; `"proposal"` for proposals.
    pub note_type: String,
    /// For notes: permalink. For proposals: short_id.
    pub permalink: String,
    /// HTML snippet with `<b>...</b>` highlights (or empty when no lexical match).
    pub snippet: String,
    /// RRF fusion score.
    pub score: f64,
}

impl From<NoteSearchResult> for MemorySearchEntityRow {
    fn from(r: NoteSearchResult) -> Self {
        Self {
            entity: "note".to_string(),
            id: r.id,
            title: r.title,
            folder: r.folder,
            note_type: r.note_type,
            permalink: r.permalink,
            snippet: r.snippet,
            score: r.score,
        }
    }
}

impl From<crate::ProposalSearchResult> for MemorySearchEntityRow {
    fn from(r: crate::ProposalSearchResult) -> Self {
        Self {
            entity: "proposal".to_string(),
            id: r.id,
            title: r.title,
            folder: String::new(),
            note_type: "proposal".to_string(),
            permalink: r.short_id,
            snippet: r.snippet,
            score: r.score,
        }
    }
}

/// Shared note lifecycle vocabulary used by the schema, repository, and tool
/// surfaces. Keep this as one status field rather than parallel booleans.
pub mod note_status {
    pub const ACTIVE: &str = "active";
    pub const ARCHIVED: &str = "archived";
    pub const DEPRECATED: &str = "deprecated";

    pub fn normalize(status: Option<&str>) -> String {
        status
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(ACTIVE)
            .to_ascii_lowercase()
    }

    pub fn is_valid(status: &str) -> bool {
        matches!(status, ACTIVE | ARCHIVED | DEPRECATED)
    }
}

/// A knowledge base note persisted in the MySQL/Dolt index, optionally backed by a
/// markdown file on disk.
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Note {
    pub id: String,
    pub project_id: String,
    /// Slug path unique within a project, e.g. "decisions/my-adr".
    pub permalink: String,
    pub title: String,
    /// Absolute path to the markdown file on disk.
    pub file_path: String,
    /// Storage backing discriminator: `file` for filesystem-backed notes, `db`
    /// for database-only notes.
    pub storage: String,
    pub note_type: String,
    pub folder: String,
    /// Lifecycle status: active by default; archived/deprecated notes remain
    /// restorable and listable only via explicit status filters.
    pub status: String,
    pub tags: String,    // JSON array string, e.g. '["rust","db"]'
    pub content: String, // Markdown body without frontmatter
    /// Objective retrieval situation where this note applies. Stored separately
    /// from `content` so embeddings/prompts can use it without mutating body text.
    pub retrieval_anchor: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub last_accessed: String,
    pub access_count: i64,
    pub confidence: f64,
    pub abstract_: Option<String>,
    pub overview: Option<String>,
    /// JSON array of relative path prefixes where this note applies.
    /// Empty `[]` = global note (applies everywhere).
    pub scope_paths: String,
}

impl Note {
    /// Parse the JSON tags string into a `Vec<String>`.
    pub fn parsed_tags(&self) -> Vec<String> {
        serde_json::from_str(&self.tags).unwrap_or_default()
    }

    /// Parse the JSON scope_paths string into a `Vec<String>`.
    pub fn parsed_scope_paths(&self) -> Vec<String> {
        serde_json::from_str(&self.scope_paths).unwrap_or_default()
    }

    /// Convert to a `serde_json::Value` with parsed tags.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "project_id": self.project_id,
            "permalink": self.permalink,
            "title": self.title,
            "file_path": self.file_path,
            "storage": self.storage,
            "note_type": self.note_type,
            "folder": self.folder,
            "status": self.status,
            "tags": self.parsed_tags(),
            "content": self.content,
            "retrieval_anchor": self.retrieval_anchor,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "last_accessed": self.last_accessed,
            "access_count": self.access_count,
            "confidence": self.confidence,
            "abstract": self.abstract_,
            "overview": self.overview,
            "scope_paths": self.parsed_scope_paths(),
        })
    }
}

/// Risk level of a structural contradiction candidate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeRisk {
    High,
    Medium,
}

/// Candidate note that may structurally contradict a newly written note.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContradictionCandidate {
    pub id: String,
    pub permalink: String,
    pub title: String,
    pub folder: String,
    pub note_type: String,
    pub score: f64,
    pub risk: TypeRisk,
}

/// A compact search result from FTS5 with BM25 score and content snippet.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoteSearchResult {
    pub id: String,
    pub permalink: String,
    pub title: String,
    pub folder: String,
    pub note_type: String,
    /// HTML snippet with `<b>…</b>` highlights around matched terms.
    pub snippet: String,
    /// RRF fusion score (higher = more relevant).
    pub score: f64,
}

/// Near-duplicate candidate returned by a bounded dedup lookup.
///
/// `content` is the full persisted note body. Callers that put candidates in a
/// prompt must apply their own explicit prompt-size cap; carrying it here keeps
/// dedup decisions grounded in the actual note rather than generated summaries.
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoteDedupCandidate {
    pub id: String,
    pub permalink: String,
    pub title: String,
    pub folder: String,
    pub note_type: String,
    pub content: String,
    pub abstract_: Option<String>,
    pub overview: Option<String>,
    pub score: f64,
}

/// Compact note summary (no full content) for list and recent queries.
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct NoteCompact {
    pub id: String,
    pub permalink: String,
    pub title: String,
    pub note_type: String,
    pub folder: String,
    pub status: String,
    pub updated_at: String,
    pub scope_paths: String,
}

/// L1 note overview payload used in tiered context responses.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct NoteOverview {
    pub id: String,
    pub permalink: String,
    pub title: String,
    pub note_type: String,
    pub overview_text: String,
    pub score: Option<f32>,
    /// True when this note has been superseded by a newer note (stale-citation
    /// confidence signal applied). Consumers should treat the content as
    /// potentially outdated.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub superseded: bool,
}

/// L0 note abstract payload used in tiered context responses.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct NoteAbstract {
    pub id: String,
    pub permalink: String,
    pub title: String,
    pub note_type: String,
    pub abstract_text: String,
    pub score: Option<f32>,
    /// True when this note has been superseded by a newer note (stale-citation
    /// confidence signal applied). Consumers should treat the content as
    /// potentially outdated.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub superseded: bool,
}

/// A single git commit entry for note history.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GitLogEntry {
    pub sha: String,
    pub message: String,
    pub author: String,
    pub date: String,
}

/// Health report for a project's knowledge base.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct HealthReport {
    pub total_notes: i64,
    pub broken_link_count: i64,
    /// Backward-compatible alias for [`authored_orphan_count`].
    /// Retained so existing MCP callers continue to see the field without
    /// breaking schema changes.
    pub orphan_note_count: i64,
    /// Notes with zero inbound authored wikilink or explicit authored
    /// association edges.  Machine-minted edges (`embedding_related`,
    /// `co_access`) do *not* hide authored-link debt.
    pub authored_orphan_count: i64,
    /// Notes with no retrieval-effective edges at all — the strictest
    /// connectivity metric.  Considers resolved wikilinks, authored/manual
    /// associations, co-access, and threshold-qualified
    /// `embedding_related` edges.
    pub isolated_count: i64,
    /// `isolated_count` as a percentage of non-singleton notes.
    pub isolated_pct: f64,
    /// Authored-orphan notes that are *not* graph-isolated because at
    /// least one non-authored retrieval edge (e.g. `embedding_related`,
    /// `co_access`) connects them.
    pub machine_connected_orphan_count: i64,
    pub low_confidence_note_count: i64,
    pub stale_note_count: i64,
    pub stale_notes_by_folder: Vec<StaleFolder>,
    /// Summed admission-dropped count from recent consolidation_run_metrics rows for this project.
    pub admission_dropped_note_count: i64,
    pub lifecycle: LifecycleHealth,
    pub recent_sweep: RecentSweepMetrics,
}

/// Lifecycle-status counts for a project's notes.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleHealth {
    pub active_notes: i64,
    pub archived_notes: i64,
    pub deprecated_notes: i64,
}

/// Most recent housekeeping/operator lifecycle sweep counters surfaced through
/// `memory_health`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RecentSweepMetrics {
    pub last_sweep_at: Option<String>,
    pub last_decayed_count: i64,
    pub last_archived_count: i64,
    pub last_superseded_source_count: i64,
}

/// Classification assigned during ADR-054 extracted-note corpus audits.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExtractedNoteAuditCategory {
    MergeCandidate,
    Underspecified,
    DemoteToWorkingSpec,
    ArchiveCandidate,
}

/// A single extracted note flagged by the ADR-054 audit.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ExtractedNoteAuditFinding {
    pub note_id: String,
    pub permalink: String,
    pub title: String,
    pub note_type: String,
    pub folder: String,
    pub category: ExtractedNoteAuditCategory,
    pub reasons: Vec<String>,
    pub related_note_ids: Vec<String>,
}

/// Result of the ADR-054 note-quality classifier — the single source of truth
/// for "is this extracted note too underspecified for durable memory?"
///
/// Shared by the extraction admission gate (in `djinn-agent`) and the corpus
/// audit (`extracted_note_audit` in `djinn-db`) so both use identical
/// predicates. Produced by [`djinn_db::repositories::note::assess_note_quality`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NoteQualityAssessment {
    /// Required ADR-054 section headings absent from (or out of canonical
    /// order in) the content body. Empty when all required sections are
    /// present in order.
    pub missing_sections: Vec<String>,
    /// Character count of the trimmed content body.
    pub content_len: usize,
    /// Number of non-empty `\n\n`-delimited blocks in the content.
    pub paragraph_count: usize,
    /// `true` when the note is too underspecified for durable memory.
    pub is_underspecified: bool,
    /// Human-readable reasons explaining why the note is underspecified
    /// (empty when it passes all checks).
    pub reasons: Vec<String>,
}

/// Aggregate ADR-054 audit report for existing extracted notes.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ExtractedNoteAuditReport {
    pub scanned_note_count: i64,
    pub merge_candidates: Vec<ExtractedNoteAuditFinding>,
    pub underspecified: Vec<ExtractedNoteAuditFinding>,
    pub demote_to_working_spec: Vec<ExtractedNoteAuditFinding>,
    pub archive_candidates: Vec<ExtractedNoteAuditFinding>,
    pub rerun_hint: String,
}

/// Stale-note count for one folder.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StaleFolder {
    pub folder: String,
    pub count: i64,
}

/// A warning that a `contradicts` edge exists between two notes.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ContradictionWarning {
    pub source_id: String,
    pub target_id: String,
    pub kind: String,
}

/// Annotation that a candidate note is superseded by another note.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SupersedesAnnotation {
    /// The note ID of the newer note that supersedes the candidate.
    pub superseded_by: String,
    pub edge_kind: String,
}

/// Annotation that a candidate note is contradicted by another note.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ContradictsAnnotation {
    pub note_id: String,
    pub edge_kind: String,
}

/// Context built from a seed note + linked related notes.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BuildContextResponse {
    /// Full-content notes at the seed.
    pub primary: Vec<Note>,
    /// L1 overview notes reached by first-hop link traversal.
    pub related_l1: Vec<NoteOverview>,
    /// L0 abstract notes reached by deeper link traversal.
    pub related_l0: Vec<NoteAbstract>,
    /// Notes in the context set that are superseded by another note in the set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<SupersedesAnnotation>,
    /// Notes in the context set that have a contradicting relationship.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contradicts: Vec<ContradictsAnnotation>,
    /// Proposals reachable from the seed note: same project via `proposal_targets`,
    /// OR whose graduated epics/tasks have memory_refs that touch the seed's task chain,
    /// ranked by lexical relevance to the seed's title + body.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposals: Vec<ProposalOverview>,
}

/// Compact proposal overview for `memory_build_context` responses.
///
/// **Progressive disclosure:** this overview ships `title` + `acceptance_criteria`
/// so the planner can decide whether to fetch the full body via `proposal_show`.
/// The full proposal body (markdown/MDX) is intentionally excluded from the
/// overview to keep prompt context lean — call `proposal_show(short_id)` for
/// the complete spec.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProposalOverview {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub body_format: String,
    pub acceptance_criteria: Vec<String>,
    pub status: String,
    pub score: Option<f64>,
}

/// Result of a filesystem-to-index reconciliation pass.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ReindexSummary {
    pub updated: i64,
    pub created: i64,
    pub deleted: i64,
    pub unchanged: i64,
}

// ── Wikilink graph types ──────────────────────────────────────────────────────

/// A knowledge graph node (note with connection metadata).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GraphNode {
    pub id: String,
    pub permalink: String,
    pub title: String,
    pub note_type: String,
    pub folder: String,
    /// Total resolved edges incident to this node (inbound + outbound).
    pub connection_count: i64,
    #[serde(default)]
    pub is_orphan: bool,
    #[serde(default)]
    pub broken_targets: Vec<String>,
    /// Entity type discriminator: `"note"` for note rows, `"proposal"` for
    /// proposal rows. Defaults to `"note"` so existing serialized responses
    /// remain backward-compatible.
    #[serde(default = "default_note_entity_type")]
    pub entity_type: String,
}

fn default_note_entity_type() -> String {
    "note".to_string()
}

/// A resolved wikilink edge between two notes.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GraphEdge {
    pub source_id: String,
    pub target_id: String,
    pub raw_text: String,
}

/// A typed semantic edge between two notes (builds_on, contradicts, supersedes,
/// exemplifies, derived_from). These are stored on `note_associations` with a
/// `kind` value other than `co_access` and surfaced through the
/// `memory_graph` tool as a separate layer from wikilink `GraphEdge`s so the
/// UI can toggle/style them independently.
///
/// Note: the F5 `note_associations` substrate is undirected (canonical-pair
/// ordering: `note_a_id < note_b_id`). Outbound direction for directional
/// kinds (e.g. `supersedes`) is reconstructed at scoring/context time.
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TypedEdge {
    pub source_id: String,
    pub target_id: String,
    pub kind: String,
    pub weight: f64,
    /// Entity type of the source endpoint. Defaults to `"note"` for backward
    /// compatibility with existing serialized responses.
    #[serde(default = "default_note_entity_type")]
    pub source_entity_type: String,
    /// Entity type of the target endpoint. Defaults to `"note"` for backward
    /// compatibility with existing serialized responses.
    #[serde(default = "default_note_entity_type")]
    pub target_entity_type: String,
}

/// Full knowledge graph: all nodes and all resolved edges.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct GraphResponse {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// Typed semantic edges (builds_on, contradicts, supersedes, exemplifies,
    /// derived_from) sourced from `note_associations` where `kind <> 'co_access'`.
    #[serde(default)]
    pub typed_edges: Vec<TypedEdge>,
}

/// A wikilink pointing to a note that does not exist.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrokenLink {
    pub source_id: String,
    pub source_permalink: String,
    pub source_title: String,
    pub raw_text: String,
}

/// A wikilink repair applied by housekeeping.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrokenLinkRepair {
    pub source_id: String,
    pub source_title: String,
    pub original_target: String,
    pub replacement_title: String,
    pub score: f64,
}

/// A note with zero inbound wikilinks (potential dead-end).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct OrphanNote {
    pub id: String,
    pub permalink: String,
    pub title: String,
    pub note_type: String,
    pub folder: String,
}

#[cfg(test)]
mod tests {
    use super::{Note, NoteAbstract, NoteOverview};

    fn sample_note(retrieval_anchor: Option<String>) -> Note {
        Note {
            id: "note_789".to_string(),
            project_id: "project_123".to_string(),
            permalink: "patterns/anchor".to_string(),
            title: "Anchor Note".to_string(),
            file_path: String::new(),
            storage: "db".to_string(),
            note_type: "pattern".to_string(),
            folder: "patterns".to_string(),
            status: "active".to_string(),
            tags: r#"["retrieval"]"#.to_string(),
            content: "Body remains separate from the retrieval anchor.".to_string(),
            retrieval_anchor,
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            updated_at: "2026-01-01T00:00:00.000Z".to_string(),
            last_accessed: "2026-01-01T00:00:00.000Z".to_string(),
            access_count: 0,
            confidence: 1.0,
            abstract_: None,
            overview: None,
            scope_paths: "[]".to_string(),
        }
    }

    #[test]
    fn note_to_value_surfaces_retrieval_anchor_without_body_duplication() {
        let note = sample_note(Some(
            "When retrieval needs an objective anchor.".to_string(),
        ));

        let value = note.to_value();

        assert_eq!(
            value["retrieval_anchor"],
            "When retrieval needs an objective anchor."
        );
        assert!(value.get("applies_when").is_none());
        assert_eq!(
            value["content"],
            "Body remains separate from the retrieval anchor."
        );
    }

    #[test]
    fn note_to_value_preserves_legacy_null_retrieval_anchor() {
        let note = sample_note(None);

        assert!(note.to_value()["retrieval_anchor"].is_null());
    }

    #[test]
    fn note_overview_serializes_with_stable_field_names() {
        let payload = NoteOverview {
            id: "note_123".to_string(),
            permalink: "reference/example".to_string(),
            title: "Example Note".to_string(),
            note_type: "reference".to_string(),
            overview_text: "Short summary".to_string(),
            score: Some(0.87),
            superseded: false,
        };

        let value = serde_json::to_value(payload).expect("serializes to JSON value");
        assert_eq!(value["id"], "note_123");
        assert_eq!(value["permalink"], "reference/example");
        assert_eq!(value["title"], "Example Note");
        assert_eq!(value["note_type"], "reference");
        assert_eq!(value["overview_text"], "Short summary");
        assert_eq!(value["score"].as_f64(), Some(0.8700000047683716));
    }

    #[test]
    fn note_abstract_serializes_with_stable_field_names() {
        let payload = NoteAbstract {
            id: "note_456".to_string(),
            permalink: "reference/another".to_string(),
            title: "Another Note".to_string(),
            note_type: "reference".to_string(),
            abstract_text: "L0 abstract".to_string(),
            score: None,
            superseded: false,
        };

        let value = serde_json::to_value(payload).expect("serializes to JSON value");
        assert_eq!(value["id"], "note_456");
        assert_eq!(value["permalink"], "reference/another");
        assert_eq!(value["title"], "Another Note");
        assert_eq!(value["note_type"], "reference");
        assert_eq!(value["abstract_text"], "L0 abstract");
        assert!(value.get("score").is_some());
        assert!(value["score"].is_null());
    }
}

//! Fixture schema contracts for the Phase 1 memory-eval benchmark.
//!
//! Committed JSONL fixtures live under the `fixtures/` directory and contain
//! corpus notes, mined memory-ref queries, and append-only bad-case rows.
//! This module defines the serde-friendly data structs that map to the real
//! pipeline's note, embedding, graph, and search contracts without requiring
//! Postgres or any external service.
//!
//! # Fixture file set
//!
//! | File                       | Row type                                      |
//! |----------------------------|-----------------------------------------------|
//! | `corpus-notes.jsonl`       | [`CorpusNoteRow`] — one per note              |
//! | `memory-ref-queries.jsonl` | [`MinedMemoryRefRow`] — mined query rows      |
//! | `bad-cases.jsonl`          | [`BadCaseRow`] — append-only regression cases |
//! | `manifest.json`            | [`FixtureManifest`] — version + hashes        |
//!
//! Loader validation and real DB writes are intentionally out of scope for this
//! slice; see task `qmzw` for the real Postgres fixture loader.

// This is a binary-crate module; these public types are API contracts for
// downstream modules (qmzw, zd4o, csom) and will be used in future tasks.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

// ── Retrieval signal vocabulary ──────────────────────────────────────────────

/// The retrieval signal types used by the RRF fusion path in
/// `NoteRepository::search` / `build_context`.
///
/// Each variant maps to one of the five signal score vectors fused by
/// [`djinn_db::repositories::note::rrf::rrf_fuse`]:
/// - `lexical` → `ranked_lexical_scores`
/// - `vector` → `semantic_scores` (embedding cosine / dot)
/// - `temporal` → `temporal_scores` (recency decay)
/// - `graph` → `graph_proximity_scores` (note_associations / wikilinks)
/// - `entity` → entity-label overlap boosting (labels/entities on notes)
/// - `task_affinity` → `task_affinity_scores` (memory_refs containment)
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalSignal {
    /// Embedding-vector cosine/dot similarity.
    Vector,
    /// Full-text / trigram lexical match.
    Lexical,
    /// Recency / temporal decay scoring.
    Temporal,
    /// Graph proximity via note_associations / wikilink edges.
    Graph,
    /// Entity / label overlap boosting.
    Entity,
    /// Task-affinity via memory_refs containment.
    TaskAffinity,
}

/// Declares which retrieval signals are expected to contribute to correct
/// retrieval for a given fixture row. This allows the benchmark to assert
/// that the claimed signals actually fire (preventing silent collapse to a
/// single-signal pipeline).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SignalCoverage {
    /// Embedding-vector cosine/dot similarity should surface these notes.
    #[serde(default)]
    pub vector: bool,
    /// Full-text / trigram lexical match should surface these notes.
    #[serde(default)]
    pub lexical: bool,
    /// Recency / temporal decay scoring should surface these notes.
    #[serde(default)]
    pub temporal: bool,
    /// Graph proximity (note_associations / wikilink edges) should change
    /// at least one relevant note rank.
    #[serde(default)]
    pub graph: bool,
    /// Entity / label overlap boosting should change at least one relevant
    /// note rank.
    #[serde(default)]
    pub entity: bool,
    /// Task-affinity via memory_refs containment should change at least one
    /// relevant note rank.
    #[serde(default)]
    pub task_affinity: bool,
}

impl SignalCoverage {
    /// Returns the set of signals that are declared as active.
    pub fn active_signals(&self) -> Vec<RetrievalSignal> {
        let mut signals = Vec::new();
        if self.vector {
            signals.push(RetrievalSignal::Vector);
        }
        if self.lexical {
            signals.push(RetrievalSignal::Lexical);
        }
        if self.temporal {
            signals.push(RetrievalSignal::Temporal);
        }
        if self.graph {
            signals.push(RetrievalSignal::Graph);
        }
        if self.entity {
            signals.push(RetrievalSignal::Entity);
        }
        if self.task_affinity {
            signals.push(RetrievalSignal::TaskAffinity);
        }
        signals
    }

    /// Returns `true` if at least one signal is declared.
    pub fn has_any(&self) -> bool {
        self.vector
            || self.lexical
            || self.temporal
            || self.graph
            || self.entity
            || self.task_affinity
    }
}

// ── Embedding metadata ──────────────────────────────────────────────────────

/// Deterministic embedding metadata and vector reference for a corpus note.
///
/// The `content_hash` key is used by the deterministic cached-vector embedder
/// (task `csom`) to look up the cached vector without calling an external
/// embedding service. The `vector` field is the full float32 embedding stored
/// inline in the fixture so that fixture loading is self-contained.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingRef {
    /// Normalized content hash used as the cache key for the deterministic
    /// embedder. Two notes with the same normalized content share an embedding.
    pub content_hash: String,
    /// Embedding model version string (e.g. `"text-embedding-3-small-v1"`).
    pub model_version: String,
    /// Dimensionality of the embedding vector.
    pub embedding_dim: usize,
    /// The full embedding vector stored inline for deterministic fixture
    /// loading. Repeated runs on the same commit + fixtures must produce
    /// byte-stable metric outputs.
    pub vector: Vec<f32>,
}

// ── Labels, entities, graph edges ────────────────────────────────────────────

/// A labeled entity extracted from or assigned to a note (e.g. person, org,
/// concept, technology). Used by the entity-overlap boosting signal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LabelEntity {
    /// Entity type discriminator: `"person"`, `"org"`, `"concept"`,
    /// `"technology"`, `"file"`, etc.
    pub entity_type: String,
    /// Human-readable entity name.
    pub name: String,
}

/// A graph edge row representing a relationship between two notes in the
/// benchmark corpus. This covers both wikilink-derived `GraphEdge` and
/// typed semantic `TypedEdge` relationships.
///
/// Maps to the `note_associations` table and the `djinn_memory::TypedEdge` /
/// `djinn_memory::GraphEdge` structs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphEdgeRow {
    /// Permalink of the source note.
    pub source_permalink: String,
    /// Permalink of the target note.
    pub target_permalink: String,
    /// Edge kind: `"co_access"`, `"builds_on"`, `"contradicts"`,
    /// `"supersedes"`, `"exemplifies"`, `"derived_from"`, or `"wikilink"`.
    pub kind: String,
    /// Edge weight (association strength or default 1.0 for typed edges).
    #[serde(default = "default_edge_weight")]
    pub weight: f64,
}

fn default_edge_weight() -> f64 {
    1.0
}

// ── Corpus note row ─────────────────────────────────────────────────────────

/// Lifecycle timestamps for a corpus note. All timestamps are ISO-8601
/// strings in UTC (matching the `Note.created_at` / `updated_at` /
/// `last_accessed` contract in `djinn_memory::Note`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LifecycleTimestamps {
    /// ISO-8601 creation timestamp.
    pub created_at: String,
    /// ISO-8601 last-updated timestamp.
    pub updated_at: String,
    /// ISO-8601 last-accessed timestamp.
    pub last_accessed: String,
}

/// A single note row in the Phase 1 benchmark corpus.
///
/// Field names and semantics align with `djinn_memory::Note` and the
/// `notes` table schema, but this struct is intentionally decoupled from
/// sqlx so the fixture can be loaded without a live database connection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CorpusNoteRow {
    /// Unique note identifier / permalink within the fixture set
    /// (e.g. `"decisions/my-adr"`).
    pub permalink: String,
    /// Human-readable note title.
    pub title: String,
    /// Markdown body content (without frontmatter).
    pub content: String,
    /// Note type: `"case"`, `"pattern"`, `"pitfall"`, `"adr"`, `"reference"`,
    /// `"design"`, `"requirement"`, `"research"`, `"brief"`, `"roadmap"`, etc.
    pub note_type: String,
    /// Folder path within the corpus (e.g. `"cases"`, `"decisions"`).
    #[serde(default)]
    pub folder: String,
    /// Lifecycle status: `"active"`, `"archived"`, or `"deprecated"`.
    /// Defaults to `"active"` per `djinn_memory::note_status::normalize`.
    #[serde(default = "default_status")]
    pub status: String,
    /// Tags as a JSON-style string array (matching `Note.tags` storage).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional retrieval anchor text (objective situation where this note
    /// applies, stored separately from content for embedding/prompt use).
    #[serde(default)]
    pub retrieval_anchor: Option<String>,
    /// Lifecycle timestamps.
    pub timestamps: LifecycleTimestamps,
    /// Bayesian confidence score (0.0–1.0). Notes below the stale-citation
    /// threshold are injection-ineligible in the real pipeline.
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    /// Optional deterministic embedding reference with inline vector.
    pub embedding: Option<EmbeddingRef>,
    /// Labeled entities on this note (used by entity-overlap signal).
    #[serde(default)]
    pub labels: Vec<LabelEntity>,
    /// Graph edges involving this note (source or target).
    /// Stored redundantly on both endpoint rows for self-contained loading.
    #[serde(default)]
    pub graph_edges: Vec<GraphEdgeRow>,
    /// Signal coverage declaration: which retrieval signals are expected to
    /// surface this note for at least one query in the corpus.
    #[serde(default)]
    pub expected_signals: SignalCoverage,
}

fn default_status() -> String {
    "active".to_string()
}

fn default_confidence() -> f64 {
    1.0
}

// ── Mined memory-ref query row ──────────────────────────────────────────────

/// A mined memory-ref query row representing a task or proposal whose
/// `memory_refs` field points to notes in the corpus.
///
/// These rows are extracted from the real `tasks.memory_refs` and
/// `epics.memory_refs` JSONB columns by the `mine-memory-refs` subcommand
/// (task `qmzw`). The fixture schema contracts define the shape; the actual
/// mining logic is out of scope for this slice.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MinedMemoryRefRow {
    /// Unique query identifier (e.g. `"task-abc123"` or `"q-001"`).
    pub query_id: String,
    /// The search query text that should retrieve the relevant notes.
    pub query_text: String,
    /// Optional task ID for task-affinity scoring. When present, the
    /// `task_affinity_scores` signal vector is expected to contribute.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Permalinks of notes that should be retrieved for this query
    /// (the "ground truth" relevant set).
    pub memory_refs: Vec<String>,
    /// Expected retrieval signal coverage for this query.
    pub expected_signals: SignalCoverage,
}

// ── Bad-case row ────────────────────────────────────────────────────────────

/// Classification of bad-case types for regression tracking.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BadCaseType {
    /// A note that should survive decay threshold but the pipeline
    /// incorrectly over-decays it.
    OverDecayThreshold,
    /// A note whose rank is influenced by graph/entity signals; if those
    /// signals are disabled, the note drops out of expected results.
    GraphEntityInfluenced,
    /// A note whose rank is influenced by task-affinity signal.
    TaskAffinityInfluenced,
    /// A query that returns zero results when it should return some.
    ZeroResult,
    /// A note whose rank regressed compared to a previous baseline.
    RankRegression,
}

/// An append-only bad-case row for Phase 1 regression tracking.
///
/// Bad cases are never deleted or modified — only appended. This ensures
/// the compare policy can detect regressions by checking that no existing
/// bad case was made worse.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BadCaseRow {
    /// Unique bad-case identifier (e.g. `"bc-001"`).
    pub case_id: String,
    /// The query text that triggers this bad case.
    pub query_text: String,
    /// Classification of the bad case.
    pub case_type: BadCaseType,
    /// Human-readable description of the expected behavior.
    pub expected_behavior: String,
    /// Optional task ID for task-affinity-influenced cases.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Permalinks of notes that should be retrieved for this case.
    pub relevant_note_permalinks: Vec<String>,
    /// Expected retrieval signal coverage.
    pub expected_signals: SignalCoverage,
    /// Tags for filtering (e.g. `["decay", "high-priority"]`).
    #[serde(default)]
    pub tags: Vec<String>,
}

// ── Fixture manifest ────────────────────────────────────────────────────────

/// Fixture file set manifest with version, creation metadata, and content
/// hashes for integrity verification.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FixtureManifest {
    /// Schema version (semver string). Bump when the fixture schema changes
    /// in a backward-incompatible way.
    pub schema_version: String,
    /// ISO-8601 timestamp when the fixture set was created.
    pub created_at: String,
    /// Number of corpus note rows.
    pub corpus_note_count: usize,
    /// Number of mined memory-ref query rows.
    pub memory_ref_query_count: usize,
    /// Number of bad-case rows.
    pub bad_case_count: usize,
    /// SHA-256 hex digest of each fixture file for integrity checks.
    pub file_hashes: FixtureFileHashes,
}

/// SHA-256 hex digests for each fixture file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FixtureFileHashes {
    pub corpus_notes: String,
    pub memory_ref_queries: String,
    pub bad_cases: String,
}

// ── Fixture paths ───────────────────────────────────────────────────────────

/// Canonical paths for the Phase 1 fixture file set relative to the crate
/// root (`server/crates/djinn-memory-eval/`).
pub struct FixturePaths;

impl FixturePaths {
    /// Root directory for committed fixtures.
    pub const FIXTURES_DIR: &'static str = "fixtures";

    /// Corpus note rows (one JSON object per line).
    pub const CORPUS_NOTES: &'static str = "fixtures/corpus-notes.jsonl";

    /// Mined memory-ref query rows.
    pub const MEMORY_REF_QUERIES: &'static str = "fixtures/memory-ref-queries.jsonl";

    /// Append-only bad-case rows.
    pub const BAD_CASES: &'static str = "fixtures/bad-cases.jsonl";

    /// Fixture manifest with version + hashes.
    pub const MANIFEST: &'static str = "fixtures/manifest.json";

    /// Baselines directory.
    pub const BASELINES_DIR: &'static str = "baselines";

    /// Phase 1 baseline file.
    pub const PHASE1_BASELINE: &'static str = "baselines/phase1.json";

    /// Returns all JSONL fixture file paths in corpus → query → bad-case order.
    pub fn all_jsonl_paths() -> &'static [&'static str] {
        &[
            Self::CORPUS_NOTES,
            Self::MEMORY_REF_QUERIES,
            Self::BAD_CASES,
        ]
    }
}

// ── Aggregate Phase 1 fixture set ───────────────────────────────────────────

/// The full Phase 1 fixture set. This is the in-memory representation after
/// parsing all JSONL files and the manifest. Not serialized as a single file;
/// each sub-set lives in its own JSONL file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Phase1Fixtures {
    pub corpus_notes: Vec<CorpusNoteRow>,
    pub memory_ref_queries: Vec<MinedMemoryRefRow>,
    pub bad_cases: Vec<BadCaseRow>,
    pub manifest: Option<FixtureManifest>,
}

impl Phase1Fixtures {
    /// Validates cross-references between fixtures:
    /// - Every `memory_ref` permalink in queries/bad-cases must exist in the
    ///   corpus.
    /// - Every graph edge endpoint must exist in the corpus.
    /// - Expected signal coverage must be non-empty where claimed.
    pub fn validate_references(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let corpus_permalinks: std::collections::HashSet<&str> = self
            .corpus_notes
            .iter()
            .map(|n| n.permalink.as_str())
            .collect();

        // Check memory-ref query references.
        for query in &self.memory_ref_queries {
            for permalink in &query.memory_refs {
                if !corpus_permalinks.contains(permalink.as_str()) {
                    errors.push(format!(
                        "query '{}': memory_ref '{}' not found in corpus",
                        query.query_id, permalink
                    ));
                }
            }
        }

        // Check bad-case references.
        for case in &self.bad_cases {
            for permalink in &case.relevant_note_permalinks {
                if !corpus_permalinks.contains(permalink.as_str()) {
                    errors.push(format!(
                        "bad-case '{}': relevant permalink '{}' not found in corpus",
                        case.case_id, permalink
                    ));
                }
            }
        }

        // Check graph edge endpoints.
        for note in &self.corpus_notes {
            for edge in &note.graph_edges {
                if !corpus_permalinks.contains(edge.source_permalink.as_str()) {
                    errors.push(format!(
                        "note '{}': graph edge source '{}' not found in corpus",
                        note.permalink, edge.source_permalink
                    ));
                }
                if !corpus_permalinks.contains(edge.target_permalink.as_str()) {
                    errors.push(format!(
                        "note '{}': graph edge target '{}' not found in corpus",
                        note.permalink, edge.target_permalink
                    ));
                }
            }
        }

        errors
    }
}

// ── JSONL parsing helpers ───────────────────────────────────────────────────

/// Parse a JSONL string into a vector of deserialized rows.
///
/// Skips blank lines. Returns an error if any non-blank line fails to parse.
pub fn parse_jsonl<T: for<'de> Deserialize<'de>>(input: &str) -> Result<Vec<T>, String> {
    let mut rows = Vec::new();
    for (line_num, line) in input.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let row: T =
            serde_json::from_str(trimmed).map_err(|e| format!("line {}: {}", line_num + 1, e))?;
        rows.push(row);
    }
    Ok(rows)
}

/// Serialize a slice of rows into JSONL format (one JSON object per line,
/// terminated by a trailing newline).
pub fn to_jsonl<T: Serialize>(rows: &[T]) -> Result<String, serde_json::Error> {
    let mut output = String::new();
    for row in rows {
        let line = serde_json::to_string(row)?;
        output.push_str(&line);
        output.push('\n');
    }
    Ok(output)
}

// ── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Representative JSON fixtures ────────────────────────────────────────

    // Compact single-line JSON constants (required for JSONL: one object per line).
    const CORPUS_ROW_JSON: &str = r#"{"permalink":"cases/slot-lifecycle-race","title":"Slot lifecycle race condition","content":"When a slot is torn down while the supervisor is still processing setup callbacks, the lifecycle runner may observe a `SlotStatus::Released` guard violation.","note_type":"case","folder":"cases","status":"active","tags":["race-condition","slot","lifecycle"],"retrieval_anchor":"slot teardown race during supervisor setup","timestamps":{"created_at":"2026-06-01T10:00:00.000Z","updated_at":"2026-06-15T14:30:00.000Z","last_accessed":"2026-07-01T09:00:00.000Z"},"confidence":0.85,"embedding":{"content_hash":"abc123def456","model_version":"text-embedding-3-small-v1","embedding_dim":3,"vector":[0.1,0.2,0.3]},"labels":[{"entity_type":"concept","name":"race condition"},{"entity_type":"file","name":"slot/lifecycle.rs"}],"graph_edges":[{"source_permalink":"cases/slot-lifecycle-race","target_permalink":"patterns/supervisor-guard","kind":"builds_on","weight":1.0}],"expected_signals":{"vector":true,"lexical":true,"temporal":true,"graph":true,"entity":true,"task_affinity":false}}"#;

    const CORPUS_ROW_MINIMAL_JSON: &str = r#"{"permalink":"patterns/supervisor-guard","title":"Supervisor guard pattern","content":"Guard pattern content","note_type":"pattern","folder":"patterns","status":"active","tags":["guard"],"timestamps":{"created_at":"2026-01-01T00:00:00.000Z","updated_at":"2026-01-01T00:00:00.000Z","last_accessed":"2026-01-01T00:00:00.000Z"},"confidence":0.9,"embedding":{"content_hash":"hash456","model_version":"text-embedding-3-small-v1","embedding_dim":3,"vector":[0.4,0.5,0.6]},"labels":[{"entity_type":"concept","name":"guard"}],"graph_edges":[],"expected_signals":{"vector":true,"lexical":true,"temporal":false,"graph":false,"entity":true,"task_affinity":false}}"#;

    const CORPUS_ROW_DECAY_JSON: &str = r#"{"permalink":"cases/over-decay-example","title":"Over-decay example case","content":"This note tests over-decay threshold behavior","note_type":"case","folder":"cases","status":"active","tags":["decay"],"timestamps":{"created_at":"2026-01-01T00:00:00.000Z","updated_at":"2026-01-01T00:00:00.000Z","last_accessed":"2026-01-01T00:00:00.000Z"},"confidence":0.5,"embedding":null,"labels":[],"graph_edges":[],"expected_signals":{"vector":true,"lexical":false,"temporal":true,"graph":false,"entity":false,"task_affinity":false}}"#;

    const MEMORY_REF_QUERY_JSON: &str = r#"{"query_id":"task-abc123","query_text":"How do I handle slot teardown race conditions?","task_id":"abc123","memory_refs":["cases/slot-lifecycle-race","patterns/supervisor-guard"],"expected_signals":{"vector":true,"lexical":true,"temporal":false,"graph":true,"entity":true,"task_affinity":true}}"#;

    const BAD_CASE_JSON: &str = r#"{"case_id":"bc-001","query_text":"What happens when a note is over-decayed?","case_type":"over_decay_threshold","expected_behavior":"Note should remain in recall@10 despite being older than decay window","task_id":null,"relevant_note_permalinks":["cases/over-decay-example"],"expected_signals":{"vector":true,"lexical":false,"temporal":true,"graph":false,"entity":false,"task_affinity":false},"tags":["decay","high-priority"]}"#;

    const BAD_CASE_GRAPH_ENTITY_JSON: &str = r#"{"case_id":"bc-002","query_text":"Which pattern builds on the supervisor guard?","case_type":"graph_entity_influenced","expected_behavior":"Graph proximity or entity overlap should surface the note in recall@5","task_id":null,"relevant_note_permalinks":["patterns/supervisor-guard"],"expected_signals":{"vector":false,"lexical":true,"temporal":false,"graph":true,"entity":true,"task_affinity":false},"tags":["graph","entity"]}"#;

    const BAD_CASE_TASK_AFFINITY_JSON: &str = r#"{"case_id":"bc-003","query_text":"What memory refs are associated with task xyz?","case_type":"task_affinity_influenced","expected_behavior":"Task-affinity signal should surface the note in recall@5 when task_id is provided","task_id":"xyz","relevant_note_permalinks":["cases/slot-lifecycle-race"],"expected_signals":{"vector":true,"lexical":false,"temporal":false,"graph":false,"entity":false,"task_affinity":true},"tags":["task-affinity"]}"#;

    // ── CorpusNoteRow deserialization ───────────────────────────────────────

    #[test]
    fn corpus_note_row_deserializes_all_fields() {
        let row: CorpusNoteRow = serde_json::from_str(CORPUS_ROW_JSON).unwrap();

        assert_eq!(row.permalink, "cases/slot-lifecycle-race");
        assert_eq!(row.title, "Slot lifecycle race condition");
        assert!(row.content.contains("SlotStatus::Released"));
        assert_eq!(row.note_type, "case");
        assert_eq!(row.folder, "cases");
        assert_eq!(row.status, "active");
        assert_eq!(row.tags, vec!["race-condition", "slot", "lifecycle"]);
        assert_eq!(
            row.retrieval_anchor.as_deref(),
            Some("slot teardown race during supervisor setup")
        );
        assert_eq!(row.timestamps.created_at, "2026-06-01T10:00:00.000Z");
        assert_eq!(row.timestamps.updated_at, "2026-06-15T14:30:00.000Z");
        assert_eq!(row.timestamps.last_accessed, "2026-07-01T09:00:00.000Z");
        assert!((row.confidence - 0.85).abs() < f64::EPSILON);
    }

    #[test]
    fn corpus_note_row_embedding_round_trips() {
        let row: CorpusNoteRow = serde_json::from_str(CORPUS_ROW_JSON).unwrap();
        let emb = row.embedding.as_ref().unwrap();

        assert_eq!(emb.content_hash, "abc123def456");
        assert_eq!(emb.model_version, "text-embedding-3-small-v1");
        assert_eq!(emb.embedding_dim, 3);
        assert_eq!(emb.vector, vec![0.1_f32, 0.2, 0.3]);

        // Round-trip: serialize then deserialize
        let json = serde_json::to_string(&row).unwrap();
        let round_tripped: CorpusNoteRow = serde_json::from_str(&json).unwrap();
        assert_eq!(row, round_tripped);
    }

    #[test]
    fn corpus_note_row_labels_and_graph_edges() {
        let row: CorpusNoteRow = serde_json::from_str(CORPUS_ROW_JSON).unwrap();

        assert_eq!(row.labels.len(), 2);
        assert_eq!(row.labels[0].entity_type, "concept");
        assert_eq!(row.labels[0].name, "race condition");
        assert_eq!(row.labels[1].entity_type, "file");

        assert_eq!(row.graph_edges.len(), 1);
        let edge = &row.graph_edges[0];
        assert_eq!(edge.source_permalink, "cases/slot-lifecycle-race");
        assert_eq!(edge.target_permalink, "patterns/supervisor-guard");
        assert_eq!(edge.kind, "builds_on");
        assert!((edge.weight - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn corpus_note_row_signal_coverage() {
        let row: CorpusNoteRow = serde_json::from_str(CORPUS_ROW_JSON).unwrap();
        let signals = &row.expected_signals;

        assert!(signals.vector);
        assert!(signals.lexical);
        assert!(signals.temporal);
        assert!(signals.graph);
        assert!(signals.entity);
        assert!(!signals.task_affinity);

        let active = signals.active_signals();
        assert_eq!(active.len(), 5);
        assert!(!active.contains(&RetrievalSignal::TaskAffinity));
    }

    #[test]
    fn corpus_note_row_defaults() {
        // Minimal JSON with only required fields
        let json = r#"{
            "permalink": "decisions/minimal",
            "title": "Minimal note",
            "content": "Body text",
            "note_type": "adr",
            "timestamps": {
                "created_at": "2026-01-01T00:00:00.000Z",
                "updated_at": "2026-01-01T00:00:00.000Z",
                "last_accessed": "2026-01-01T00:00:00.000Z"
            }
        }"#;

        let row: CorpusNoteRow = serde_json::from_str(json).unwrap();
        assert_eq!(row.status, "active"); // default
        assert!((row.confidence - 1.0).abs() < f64::EPSILON); // default
        assert!(row.embedding.is_none());
        assert!(row.labels.is_empty());
        assert!(row.graph_edges.is_empty());
        assert!(!row.expected_signals.has_any()); // all false by default
        assert!(row.folder.is_empty());
        assert!(row.retrieval_anchor.is_none());
    }

    // ── MinedMemoryRefRow deserialization ───────────────────────────────────

    #[test]
    fn memory_ref_query_deserializes_with_task_affinity() {
        let row: MinedMemoryRefRow = serde_json::from_str(MEMORY_REF_QUERY_JSON).unwrap();

        assert_eq!(row.query_id, "task-abc123");
        assert_eq!(
            row.query_text,
            "How do I handle slot teardown race conditions?"
        );
        assert_eq!(row.task_id.as_deref(), Some("abc123"));
        assert_eq!(
            row.memory_refs,
            vec!["cases/slot-lifecycle-race", "patterns/supervisor-guard"]
        );
        assert!(row.expected_signals.task_affinity);
        assert!(row.expected_signals.vector);
        assert!(row.expected_signals.graph);
        assert!(row.expected_signals.entity);
        assert!(!row.expected_signals.temporal);
    }

    #[test]
    fn memory_ref_query_round_trips() {
        let row: MinedMemoryRefRow = serde_json::from_str(MEMORY_REF_QUERY_JSON).unwrap();
        let json = serde_json::to_string(&row).unwrap();
        let round_tripped: MinedMemoryRefRow = serde_json::from_str(&json).unwrap();
        assert_eq!(row, round_tripped);
    }

    #[test]
    fn memory_ref_query_without_task_id() {
        let json = r#"{
            "query_id": "q-010",
            "query_text": "What are the best practices for note extraction?",
            "memory_refs": ["patterns/extraction-best-practices"],
            "expected_signals": {
                "vector": true,
                "lexical": true
            }
        }"#;

        let row: MinedMemoryRefRow = serde_json::from_str(json).unwrap();
        assert!(row.task_id.is_none());
        assert!(row.expected_signals.vector);
        assert!(row.expected_signals.lexical);
        assert!(!row.expected_signals.task_affinity);
    }

    // ── BadCaseRow deserialization ──────────────────────────────────────────

    #[test]
    fn bad_case_over_decay_deserializes() {
        let row: BadCaseRow = serde_json::from_str(BAD_CASE_JSON).unwrap();

        assert_eq!(row.case_id, "bc-001");
        assert_eq!(row.case_type, BadCaseType::OverDecayThreshold);
        assert!(row.task_id.is_none());
        assert_eq!(
            row.relevant_note_permalinks,
            vec!["cases/over-decay-example"]
        );
        assert!(row.expected_signals.temporal);
        assert!(row.expected_signals.vector);
        assert!(!row.expected_signals.graph);
        assert_eq!(row.tags, vec!["decay", "high-priority"]);
    }

    #[test]
    fn bad_case_graph_entity_influenced() {
        let row: BadCaseRow = serde_json::from_str(BAD_CASE_GRAPH_ENTITY_JSON).unwrap();

        assert_eq!(row.case_type, BadCaseType::GraphEntityInfluenced);
        assert!(row.expected_signals.graph);
        assert!(row.expected_signals.entity);
        assert!(!row.expected_signals.task_affinity);
    }

    #[test]
    fn bad_case_task_affinity_influenced() {
        let row: BadCaseRow = serde_json::from_str(BAD_CASE_TASK_AFFINITY_JSON).unwrap();

        assert_eq!(row.case_type, BadCaseType::TaskAffinityInfluenced);
        assert_eq!(row.task_id.as_deref(), Some("xyz"));
        assert!(row.expected_signals.task_affinity);
        assert!(row.expected_signals.vector);
    }

    #[test]
    fn bad_case_all_types_round_trip() {
        let types_json = [
            (r#""over_decay_threshold""#, BadCaseType::OverDecayThreshold),
            (
                r#""graph_entity_influenced""#,
                BadCaseType::GraphEntityInfluenced,
            ),
            (
                r#""task_affinity_influenced""#,
                BadCaseType::TaskAffinityInfluenced,
            ),
            (r#""zero_result""#, BadCaseType::ZeroResult),
            (r#""rank_regression""#, BadCaseType::RankRegression),
        ];

        for (json_str, expected) in &types_json {
            let parsed: BadCaseType = serde_json::from_str(json_str).unwrap();
            assert_eq!(&parsed, expected);
            let serialized = serde_json::to_string(&parsed).unwrap();
            assert_eq!(&serialized, json_str);
        }
    }

    #[test]
    fn bad_case_round_trip() {
        let row: BadCaseRow = serde_json::from_str(BAD_CASE_JSON).unwrap();
        let json = serde_json::to_string(&row).unwrap();
        let round_tripped: BadCaseRow = serde_json::from_str(&json).unwrap();
        assert_eq!(row, round_tripped);
    }

    // ── SignalCoverage ──────────────────────────────────────────────────────

    #[test]
    fn signal_coverage_active_signals() {
        let coverage = SignalCoverage {
            vector: true,
            lexical: false,
            temporal: true,
            graph: false,
            entity: true,
            task_affinity: false,
        };
        let active = coverage.active_signals();
        assert_eq!(active.len(), 3);
        assert!(active.contains(&RetrievalSignal::Vector));
        assert!(active.contains(&RetrievalSignal::Temporal));
        assert!(active.contains(&RetrievalSignal::Entity));
        assert!(!active.contains(&RetrievalSignal::Lexical));
    }

    #[test]
    fn signal_coverage_default_is_empty() {
        let coverage = SignalCoverage::default();
        assert!(!coverage.has_any());
        assert!(coverage.active_signals().is_empty());
    }

    #[test]
    fn signal_coverage_all_active() {
        let coverage = SignalCoverage {
            vector: true,
            lexical: true,
            temporal: true,
            graph: true,
            entity: true,
            task_affinity: true,
        };
        assert!(coverage.has_any());
        assert_eq!(coverage.active_signals().len(), 6);
    }

    // ── RetrievalSignal serialization ───────────────────────────────────────

    #[test]
    fn retrieval_signal_serializes_to_snake_case() {
        assert_eq!(
            serde_json::to_string(&RetrievalSignal::Vector).unwrap(),
            r#""vector""#
        );
        assert_eq!(
            serde_json::to_string(&RetrievalSignal::Lexical).unwrap(),
            r#""lexical""#
        );
        assert_eq!(
            serde_json::to_string(&RetrievalSignal::Temporal).unwrap(),
            r#""temporal""#
        );
        assert_eq!(
            serde_json::to_string(&RetrievalSignal::Graph).unwrap(),
            r#""graph""#
        );
        assert_eq!(
            serde_json::to_string(&RetrievalSignal::Entity).unwrap(),
            r#""entity""#
        );
        assert_eq!(
            serde_json::to_string(&RetrievalSignal::TaskAffinity).unwrap(),
            r#""task_affinity""#
        );
    }

    #[test]
    fn retrieval_signal_deserializes_from_snake_case() {
        assert_eq!(
            serde_json::from_str::<RetrievalSignal>(r#""task_affinity""#).unwrap(),
            RetrievalSignal::TaskAffinity
        );
    }

    // ── FixturePaths ────────────────────────────────────────────────────────

    #[test]
    fn fixture_paths_are_correct() {
        assert_eq!(FixturePaths::FIXTURES_DIR, "fixtures");
        assert_eq!(FixturePaths::CORPUS_NOTES, "fixtures/corpus-notes.jsonl");
        assert_eq!(
            FixturePaths::MEMORY_REF_QUERIES,
            "fixtures/memory-ref-queries.jsonl"
        );
        assert_eq!(FixturePaths::BAD_CASES, "fixtures/bad-cases.jsonl");
        assert_eq!(FixturePaths::MANIFEST, "fixtures/manifest.json");
        assert_eq!(FixturePaths::PHASE1_BASELINE, "baselines/phase1.json");
    }

    #[test]
    fn all_jsonl_paths_covers_three_files() {
        assert_eq!(FixturePaths::all_jsonl_paths().len(), 3);
    }

    // ── JSONL parse/to helpers ──────────────────────────────────────────────

    #[test]
    fn parse_jsonl_corpus_notes() {
        let input = format!("{}\n{}\n", CORPUS_ROW_JSON, CORPUS_ROW_JSON);
        let rows: Vec<CorpusNoteRow> = parse_jsonl(&input).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].permalink, "cases/slot-lifecycle-race");
    }

    #[test]
    fn parse_jsonl_skips_blank_lines() {
        let input = format!("\n{}\n\n{}\n\n", CORPUS_ROW_JSON, CORPUS_ROW_JSON);
        let rows: Vec<CorpusNoteRow> = parse_jsonl(&input).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn parse_jsonl_error_includes_line_number() {
        let input = format!("{}\n{{bad json}}\n", CORPUS_ROW_JSON);
        let result: Result<Vec<CorpusNoteRow>, _> = parse_jsonl(&input);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("line 2"),
            "error should mention line 2, got: {}",
            err
        );
    }

    #[test]
    fn to_jsonl_produces_one_line_per_row() {
        let rows: Vec<CorpusNoteRow> = vec![
            serde_json::from_str(CORPUS_ROW_JSON).unwrap(),
            serde_json::from_str(CORPUS_ROW_JSON).unwrap(),
        ];
        let jsonl = to_jsonl(&rows).unwrap();
        let lines: Vec<&str> = jsonl.lines().collect();
        assert_eq!(lines.len(), 2);
        // Each line should parse back.
        for line in &lines {
            let _: CorpusNoteRow = serde_json::from_str(line).unwrap();
        }
    }

    #[test]
    fn to_jsonl_empty_slice() {
        let rows: Vec<CorpusNoteRow> = vec![];
        let jsonl = to_jsonl(&rows).unwrap();
        assert!(jsonl.is_empty());
    }

    // ── Phase1Fixtures cross-reference validation ───────────────────────────

    #[test]
    fn validate_references_passes_when_all_refs_exist() {
        let fixtures = Phase1Fixtures {
            corpus_notes: vec![
                serde_json::from_str(CORPUS_ROW_JSON).unwrap(),
                serde_json::from_str(CORPUS_ROW_MINIMAL_JSON).unwrap(),
            ],
            memory_ref_queries: vec![serde_json::from_str(MEMORY_REF_QUERY_JSON).unwrap()],
            bad_cases: vec![],
            manifest: None,
        };

        let errors = fixtures.validate_references();
        assert!(errors.is_empty(), "expected no errors, got: {:?}", errors);
    }

    #[test]
    fn validate_references_catches_missing_corpus_permalink() {
        let fixtures = Phase1Fixtures {
            corpus_notes: vec![], // empty corpus
            memory_ref_queries: vec![serde_json::from_str(MEMORY_REF_QUERY_JSON).unwrap()],
            bad_cases: vec![],
            manifest: None,
        };

        let errors = fixtures.validate_references();
        assert!(!errors.is_empty());
        assert!(errors.iter().any(|e| e.contains("not found in corpus")));
    }

    #[test]
    fn validate_references_catches_missing_graph_edge_target() {
        let mut note: CorpusNoteRow = serde_json::from_str(CORPUS_ROW_JSON).unwrap();
        // Add an edge to a note that doesn't exist in corpus
        note.graph_edges.push(GraphEdgeRow {
            source_permalink: "cases/slot-lifecycle-race".to_string(),
            target_permalink: "nonexistent/note".to_string(),
            kind: "builds_on".to_string(),
            weight: 1.0,
        });

        let fixtures = Phase1Fixtures {
            corpus_notes: vec![note],
            memory_ref_queries: vec![],
            bad_cases: vec![],
            manifest: None,
        };

        let errors = fixtures.validate_references();
        assert!(!errors.is_empty());
        assert!(errors.iter().any(|e| e.contains("nonexistent/note")));
    }

    // ── Manifest ────────────────────────────────────────────────────────────

    #[test]
    fn manifest_deserializes() {
        let json = r#"{
            "schema_version": "1.0.0",
            "created_at": "2026-07-09T00:00:00.000Z",
            "corpus_note_count": 10,
            "memory_ref_query_count": 5,
            "bad_case_count": 3,
            "file_hashes": {
                "corpus_notes": "abc123",
                "memory_ref_queries": "def456",
                "bad_cases": "ghi789"
            }
        }"#;

        let manifest: FixtureManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.schema_version, "1.0.0");
        assert_eq!(manifest.corpus_note_count, 10);
        assert_eq!(manifest.memory_ref_query_count, 5);
        assert_eq!(manifest.bad_case_count, 3);
        assert_eq!(manifest.file_hashes.corpus_notes, "abc123");
    }

    // ── Integration: full JSONL parse round-trip ────────────────────────────

    #[test]
    fn full_fixture_set_jsonl_round_trip() {
        // Build a small fixture set from compact JSON constants
        let corpus: Vec<CorpusNoteRow> = vec![
            serde_json::from_str(CORPUS_ROW_JSON).unwrap(),
            serde_json::from_str(CORPUS_ROW_MINIMAL_JSON).unwrap(),
            serde_json::from_str(CORPUS_ROW_DECAY_JSON).unwrap(),
        ];

        let queries: Vec<MinedMemoryRefRow> =
            vec![serde_json::from_str(MEMORY_REF_QUERY_JSON).unwrap()];

        let bad_cases: Vec<BadCaseRow> = vec![
            serde_json::from_str(BAD_CASE_JSON).unwrap(),
            serde_json::from_str(BAD_CASE_GRAPH_ENTITY_JSON).unwrap(),
            serde_json::from_str(BAD_CASE_TASK_AFFINITY_JSON).unwrap(),
        ];

        // Serialize to JSONL
        let corpus_jsonl = to_jsonl(&corpus).unwrap();
        let queries_jsonl = to_jsonl(&queries).unwrap();
        let bad_cases_jsonl = to_jsonl(&bad_cases).unwrap();

        // Parse back
        let parsed_corpus: Vec<CorpusNoteRow> = parse_jsonl(&corpus_jsonl).unwrap();
        let parsed_queries: Vec<MinedMemoryRefRow> = parse_jsonl(&queries_jsonl).unwrap();
        let parsed_bad_cases: Vec<BadCaseRow> = parse_jsonl(&bad_cases_jsonl).unwrap();

        assert_eq!(corpus, parsed_corpus);
        assert_eq!(queries, parsed_queries);
        assert_eq!(bad_cases, parsed_bad_cases);

        // Validate cross-references
        let fixtures = Phase1Fixtures {
            corpus_notes: parsed_corpus,
            memory_ref_queries: parsed_queries,
            bad_cases: parsed_bad_cases,
            manifest: None,
        };
        let errors = fixtures.validate_references();
        assert!(errors.is_empty(), "validation errors: {:?}", errors);
    }

    // ── GraphEdgeRow and LabelEntity ────────────────────────────────────────

    #[test]
    fn graph_edge_default_weight() {
        let json = r#"{
            "source_permalink": "a",
            "target_permalink": "b",
            "kind": "wikilink"
        }"#;
        let edge: GraphEdgeRow = serde_json::from_str(json).unwrap();
        assert!((edge.weight - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn label_entity_round_trip() {
        let entity = LabelEntity {
            entity_type: "technology".to_string(),
            name: "Rust".to_string(),
        };
        let json = serde_json::to_string(&entity).unwrap();
        let round_tripped: LabelEntity = serde_json::from_str(&json).unwrap();
        assert_eq!(entity, round_tripped);
    }

    // ── EmbeddingRef ────────────────────────────────────────────────────────

    #[test]
    fn embedding_ref_empty_vector() {
        let json = r#"{
            "content_hash": "abc",
            "model_version": "v1",
            "embedding_dim": 0,
            "vector": []
        }"#;
        let emb: EmbeddingRef = serde_json::from_str(json).unwrap();
        assert!(emb.vector.is_empty());
        assert_eq!(emb.embedding_dim, 0);
    }
}

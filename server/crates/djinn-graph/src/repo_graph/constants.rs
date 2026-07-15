//! Tuning constants for the repo-graph module.
//!
//! All values are [`pub(crate)`] so the sibling submodules (`node`, `edge`,
//! `graph`, `ranking`, `builder`, `artifact`, `mod`, `tests`) can reach
//! them. External consumers should not depend on these numbers — they are
//! implementation details that may shift as the PageRank / edge scoring
//! recipes evolve.

pub(crate) const PAGE_RANK_DAMPING_FACTOR: f64 = 0.85;
pub(crate) const PAGE_RANK_ITERATIONS: usize = 25;

/// Repo-graph artifact schema version.
///
/// Bumped when the on-disk shape (struct fields, enum variants) changes in
/// ways that would silently corrupt a bincode load. Old blobs that do not
/// carry this field — or carry a lower value — are rejected by
/// [`crate::repo_graph::RepoDependencyGraph::from_artifact`] (via bincode
/// failure on the new field set) and force a re-warm.
///
/// Bump history:
/// - v1 (initial): added `confidence` and `reason` to every edge (PR A2).
/// - v2: split `SymbolReference` into `Reads` / `Writes` based on SCIP
///   `SymbolRole::ReadAccess` / `WriteAccess` flags (PR A3). Old blobs
///   bincode-deserialize but their edges are stamped with the legacy
///   `SymbolReference` kind only — next warm rebuilds with the split.
/// - v3: entry-point detection (PR F1) — adds `EntryPointOf` edge kind
///   and `is_test` flag on `RepoGraphNode`. Old v2 blobs bincode-fail on
///   the new edge variant / extra node field and trigger a re-warm.
/// - v4: persist the [`crate::communities::Community`] sidecar in the
///   artifact and add the [`crate::repo_graph::RepoGraphEdgeKind::MemberOf`]
///   variant (PR F3). Old blobs bincode-deserialize with empty
///   communities; next warm runs greedy modularity detection and
///   populates them.
/// - v5: process (execution flow) detection (PR F2) — adds
///   `RepoGraphEdgeKind::StepInProcess`, `RepoGraphNodeKind::Process`,
///   `RepoNodeKey::Process(String)`, an optional `step` ordinal on each
///   edge, and a `processes: Vec<Process>` sidecar on the artifact. Old
///   v4 blobs bincode-fail on the new variants / extra fields and force
///   a re-warm.
/// - v6: rename the four `SymbolRelationship*` edge variants to their
///   semantic names (`Extends`, `Implements`, `TypeDefines`, `Defines`).
///   The on-wire bincode positional encoding is unchanged (variant
///   order preserved), but the serde rename surface and public JSON
///   field names shift, so the version stamp is bumped to communicate
///   the public-API break to any consumers parsing serialized output.
/// - v7: DB-access detection — adds `RepoGraphNodeKind::Table` and
///   `RepoNodeKey::Table(String)` for synthetic database-table nodes,
///   plus `Reads`/`Writes` edges from caller symbols to table nodes.
///   Old v6 blobs bincode-fail on the new `RepoNodeKey` / kind
///   variants and trigger a re-warm.
/// - v8: drop function/method-scoped `Variable`/`Parameter` SCIP symbols at
///   parse time to avoid super-nodes (every `ctx`/`err`/`logger` across the
///   repo collapsing into one). Old v7 blobs still bincode-deserialize but
///   contain the polluted node set; the version bump forces a re-warm so
///   the on-disk cache reflects the cleaner graph. Filter predicate:
///   see `crate::scip_parser::is_function_scoped_variable`.
/// - v9: per-function complexity metrics (iteration 26) — adds
///   `complexity: Option<ComplexityMetrics>` to every `RepoGraphNode`.
///   Populated for function-like symbols whose host file the
///   [`crate::complexity::ComplexityWalker`] can parse (currently Rust;
///   more languages land in iter 24/25). Old v8 blobs do not carry
///   the field; `#[serde(default)]` lets them deserialize as `None`,
///   but the version bump still forces a re-warm so caches reflect
///   the freshly-computed metrics rather than running indefinitely
///   with `None` everywhere.
/// - v10: canonical test classification — `is_test` is now populated
///   for File nodes and for every Symbol node via the file-path
///   convention ([`is_test_path`]), OR-ed with the pre-existing SCIP
///   `Test`-role signal. Previously `is_test` was symbol-only and set
///   solely from the SCIP role (which most indexers never stamp), and
///   File nodes hardcoded `false`. Old v9 blobs still deserialize but
///   carry the under-populated flag; the bump forces a re-warm so the
///   `/code-graph` "hide tests" toggle and the `code_graph tests=`
///   filter see a complete classification.
/// - v11: adds `TraitDispatchCall` edge kind for synthesized
///   trait-dispatch caller edges (proposal t16t). Old v10 blobs
///   bincode-fail on the new enum variant and force a re-warm so
///   the graph reflects persisted trait-dispatch semantics.
///
/// Route/Tool nodes plus `HandlesRoute` / `Fetches` route edges are
/// intentionally additive under v10: the new enum variants are appended,
/// new struct fields are `#[serde(default)]`, and existing v10 artifacts that
/// do not contain the new surface keep round-tripping without a forced re-warm.
pub const REPO_GRAPH_ARTIFACT_VERSION: u32 = 11;

/// Canonical "is this path a test file" classification — re-exported
/// from [`djinn_core::test_paths`] (the single source of truth) so the
/// build-time `RepoGraphNode::is_test` stamping uses the exact same rule
/// as the control-plane `code_graph tests=` filter and the agent's
/// blast-radius categoriser.
pub use djinn_core::test_paths::is_test_path;

// ── Edge confidence floor table (PR A2) ────────────────────────────────────
//
// Initial confidence assigned to every edge of a given kind. The visibility
// heuristic (a `local `-prefixed source or target symbol) lowers the floor by
// `EDGE_CONFIDENCE_LOCAL_PENALTY` and stamps `reason="local-prefix"` on the
// edge so downstream filters can explain themselves.

pub(crate) const EDGE_CONFIDENCE_CONTAINS_DEFINITION: f64 = 0.95;
pub(crate) const EDGE_CONFIDENCE_DECLARED_IN_FILE: f64 = 0.95;
pub(crate) const EDGE_CONFIDENCE_FILE_REFERENCE: f64 = 0.85;
pub(crate) const EDGE_CONFIDENCE_SYMBOL_REFERENCE: f64 = 0.90;
pub(crate) const EDGE_CONFIDENCE_EXTENDS: f64 = 0.80;
pub(crate) const EDGE_CONFIDENCE_IMPLEMENTS: f64 = 0.85;
pub(crate) const EDGE_CONFIDENCE_TYPE_DEFINES: f64 = 0.85;
pub(crate) const EDGE_CONFIDENCE_DEFINES: f64 = 0.85;
// PR A3: split confidences for `Reads` / `Writes` (carved out of
// `SymbolReference`). Writes are the more reliable signal because SCIP's
// `WriteAccess` flag is set deterministically by the indexer at the
// assignment site; reads cover both load/use sites and method-call
// receivers, so they sit slightly lower. The plan didn't pin numbers —
// we use 0.90 / 0.85 so `Writes` matches the old `SymbolReference`
// floor (no regression for write-detection downstream) and `Reads` takes
// a one-tier penalty.
pub(crate) const EDGE_CONFIDENCE_READS: f64 = 0.85;
pub(crate) const EDGE_CONFIDENCE_WRITES: f64 = 0.90;
// Inferred route/API edges are string-shape signals unless an extractor stamps
// stronger evidence in the edge reason. Keep their floor below deterministic
// SCIP edges so the confidence tier can distinguish inferred suggestions from
// extracted graph facts without adding persisted fields to artifacts.
pub(crate) const EDGE_CONFIDENCE_ROUTE: f64 = 0.75;
// PR F1: floor for `EntryPointOf` edges. The detector itself records
// per-hit confidence in [0.6, 0.95] depending on signal strength
// (`fn main`, SCIP `Test` role → 0.95; file-path heuristics → 0.7;
// import-shape heuristics → 0.6). The floor only matters when the edge
// is added with a confidence below 0.5 — we set it to 0.5 so the table
// stays consistent with the rest of the file. Per-hit confidences
// override the floor in [`detect_entry_points`].
pub(crate) const EDGE_CONFIDENCE_ENTRY_POINT_OF: f64 = 0.5;
// PR F3: synthesized `Community` membership edge — confidence floor
// 0.95 since the modularity partition is deterministic for a given
// graph. Same tier as `ContainsDefinition` / `DeclaredInFile` (also
// algorithmically derived from SCIP, not sampled).
pub(crate) const EDGE_CONFIDENCE_MEMBER_OF: f64 = 0.95;
// PR F2: `StepInProcess` edges are synthetic links from a `Process`
// node to each step in the deterministic call chain it traces. They
// carry the same 0.95 floor as `ContainsDefinition` / `DeclaredInFile`
// — the partition is computed from the SCIP-derived edge structure, so
// every `StepInProcess` is as trustworthy as the strongest source edge
// the trace consumed.
pub(crate) const EDGE_CONFIDENCE_STEP_IN_PROCESS: f64 = 0.95;
// PR s6ch / ykcg route model: `HandlesRoute` comes from deterministic
// server-side route detector output (route node -> handler symbol), so it
// sits in the high-confidence metadata tier. `Fetches` is client-side
// consumer inference (caller symbol -> route node) and is deliberately lower
// confidence than `HandlesRoute`.
pub(crate) const EDGE_CONFIDENCE_HANDLES_ROUTE: f64 = 0.90;
pub(crate) const EDGE_CONFIDENCE_FETCHES: f64 = 0.75;
// Trait-dispatch edges are synthesized from SCIP implementation/occurrence
// analysis rather than directly extracted. They sit in the inferred tier
// (below 0.9) so `confidence_tier()` classifies them as `Inferred` unless
// the confidence is dropped below the floor (→ `Ambiguous`).
//
// The 0.70 floor is deliberately below the default `impact` min_confidence
// (0.85), so synthesized trait-dispatch callers are excluded from blast-
// radius results unless the caller explicitly lowers the threshold.
// See `docs/TRAIT_DISPATCH_VALIDATION.md` for the full semantics.
pub(crate) const EDGE_CONFIDENCE_TRAIT_DISPATCH_CALL: f64 = 0.70;
// Proposal qoxm: commit co-change coupling edges. Confidence carries the
// coupling *score* (a saturating function of the co-change count), never
// SCIP proof — co-change is circumstantial evidence, so the tier is capped at
// `Inferred`. This floor is the Inferred/Ambiguous band boundary: edges whose
// coupling score lands below it classify as `Ambiguous`; at or above it they
// are `Inferred` (and NEVER `Extracted`). See `edge_confidence_tier`.
pub(crate) const EDGE_CONFIDENCE_CO_CHANGED_WITH: f64 = 0.5;
pub(crate) const EDGE_CONFIDENCE_LOCAL_PENALTY: f64 = 0.15;
pub(crate) const EDGE_WEIGHT_DEFINITION_TO_FILE: f64 = 4.0;
pub(crate) const EDGE_WEIGHT_FILE_TO_DEFINITION: f64 = 1.5;
pub(crate) const EDGE_WEIGHT_FILE_REFERENCE: f64 = 2.5;
pub(crate) const EDGE_WEIGHT_SYMBOL_REFERENCE: f64 = 3.5;
pub(crate) const EDGE_WEIGHT_ROUTE: f64 = 2.0;
pub(crate) const EDGE_WEIGHT_EXTENDS: f64 = 2.0;
pub(crate) const EDGE_WEIGHT_IMPLEMENTS: f64 = 2.5;
pub(crate) const EDGE_WEIGHT_TYPE_DEFINES: f64 = 1.75;
pub(crate) const EDGE_WEIGHT_DEFINES: f64 = 2.25;
// PR F1: keep `EntryPointOf` light — the edge is metadata, not a
// dependency signal, so it should not perturb PageRank or shortest-path
// scoring.
pub(crate) const EDGE_WEIGHT_ENTRY_POINT_OF: f64 = 0.5;
// PR F3: `MemberOf` edges are structural (not weighted by SCIP
// evidence count), so they get a constant low weight that doesn't
// dominate PageRank. The community is a side-channel; it shouldn't
// reshape the importance ranking.
pub(crate) const EDGE_WEIGHT_MEMBER_OF: f64 = 1.0;
// PR F2: `StepInProcess` edges are structural metadata (not new SCIP
// evidence), so they get a constant low weight that does not dominate
// PageRank or A* shortest-path queries. Process nodes are a side-
// channel: they should not reshape the importance ranking of the
// underlying call graph.
pub(crate) const EDGE_WEIGHT_STEP_IN_PROCESS: f64 = 0.5;
// PR s6ch / ykcg route model: route edges are metadata / side-channel links,
// not primary callgraph evidence. Keep them low-to-moderate so route maps can
// hang off the graph without dominating PageRank or shortest-path scoring.
pub(crate) const EDGE_WEIGHT_HANDLES_ROUTE: f64 = 0.75;
pub(crate) const EDGE_WEIGHT_FETCHES: f64 = 1.0;
// Trait-dispatch call edges are synthesized call-graph links (caller →
// trait-method). They carry moderate weight — stronger than metadata-only
// edges (`EntryPointOf`, `MemberOf`) but below primary SCIP evidence
// (`Implements`, `SymbolReference`) so they don't dominate PageRank.
pub(crate) const EDGE_WEIGHT_TRAIT_DISPATCH_CALL: f64 = 1.5;
// Proposal qoxm: co-change edges live in a dedicated sidecar *outside* the
// PageRank/traversal petgraph, so this weight never actually perturbs ranking
// or shortest-path scoring. It exists only to satisfy the exhaustive
// `edge_weight` table; keep it light to reflect that co-change is a
// side-channel signal, not primary call-graph evidence.
pub(crate) const EDGE_WEIGHT_CO_CHANGED_WITH: f64 = 0.5;
// ── Trait-dispatch reason constants (proposal t16t) ───────────────────────
//
// Stable reason strings stamped on synthesized trait-dispatch edges so
// downstream consumers can distinguish provenance without string-matching
// fragile prose.

/// Reason stamped on the direct caller → trait-method edge when a SCIP
/// occurrence resolves to a trait method symbol via dispatch analysis.
pub const REASON_TRAIT_DISPATCH_CALL: &str = "trait-dispatch-call";

/// Reason stamped on trait-method → implementation edges during bounded
/// fan-out (future work; the reason constant is defined here so builder
/// code can use it immediately).
pub const REASON_TRAIT_DISPATCH_FANOUT: &str = "trait-dispatch-fanout";

/// Reason stamped when a potential fan-out edge is suppressed (fan-out cap
/// reached, ambiguous impl, etc.).
pub const REASON_TRAIT_DISPATCH_SUPPRESSED: &str = "trait-dispatch-suppressed";

/// Maximum number of concrete implementation methods to which a caller →
/// trait-method dispatch edge will fan out. When a trait method has more
/// known implementations than this cap, the builder suppresses the
/// synthesized fan-out edges entirely to prevent unbounded edge
/// multiplication (e.g. `RuntimeOps` with 10+ impls). The direct caller
/// → trait-method `TraitDispatchCall` edge is still emitted regardless of
/// the cap; only the impl-fanout edges are suppressed.
///
/// The cap is deliberately conservative: most traits in real Rust codebases
/// have fewer than 5 implementations, so a cap of 5 covers the common case
/// while preventing pathological multiplication for widely-implemented
/// framework traits.
///
/// See `docs/TRAIT_DISPATCH_VALIDATION.md` for the full validation matrix
/// and confidence/provenance semantics.
pub const TRAIT_DISPATCH_FANOUT_CAP: usize = 5;

pub(crate) const SYMBOL_KIND_TYPE_MULTIPLIER: f64 = 1.15;
pub(crate) const SYMBOL_KIND_METHOD_MULTIPLIER: f64 = 1.05;
pub(crate) const SYMBOL_KIND_FUNCTION_MULTIPLIER: f64 = 1.0;
pub(crate) const SYMBOL_KIND_VARIABLE_MULTIPLIER: f64 = 0.7;
pub(crate) const SYMBOL_KIND_DEFAULT_MULTIPLIER: f64 = 0.9;

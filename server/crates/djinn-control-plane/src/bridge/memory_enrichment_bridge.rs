//! Bridge between `djinn-control-plane` and the `djinn-agent` memory-enrichment
//! pass.
//!
//! ## Why a trait?
//!
//! `djinn-agent` depends on `djinn-control-plane`, not the other way around.
//! The control-plane MCP tool (`memory_run_enrichment`) needs to invoke
//! `djinn_agent::actors::slot::memory_enrichment::run_memory_enrichment`, but
//! the dependency direction forbids a direct call.
//!
//! Following the same `Arc<dyn Trait>` pattern as `RepoGraphOps` and friends
//! (see `bridge/graph_bridge.rs`), the MCP layer is generic over the trait
//! shape and the server binary wires a concrete implementation that delegates
//! to the agent. This is the smallest shared abstraction that satisfies the
//! constraint — see the concurrency regression test for the documented
//! contract.
//!
//! ## Wire types
//!
//! The wire types mirror `djinn_agent::actors::slot::memory_enrichment::*` so
//! the MCP tool doesn't have to plumb agent types through its public
//! surface. The server-side implementation translates between the two
//! representations at the bridge boundary.
//!
//! Proposal-aware enrichment (Wave 3 / rbsi): the bridge surfaces the
//! proposal-aware fields (`source_proposal_id` / `target_proposal_id` /
//! `source_kind` / `target_kind`) added by qb9o. Note↔note edges continue to
//! populate the legacy `source_note_id` / `target_note_id` fields; edges
//! with one or both endpoints on a proposal populate the matching
//! `*_proposal_id` field and leave the note-id field empty.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Entity node extracted by the enrichment pass — recurring system or concept
/// ("dispatch gate", "circuit breaker", "slot actor").
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct EnrichmentEntity {
    pub canonical_name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// Claim node extracted by the enrichment pass — a decision or assertion the
/// memory records.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct EnrichmentClaim {
    pub statement: String,
    pub source_note_id: String,
    #[serde(default)]
    pub evidence_quote: Option<String>,
}

/// Endpoint kind tag for a typed enrichment edge.
///
/// Note↔note edges use `Note` on both sides; note↔proposal and
/// proposal↔proposal edges use `Proposal` on the heterogeneous substrate
/// (memory_entity_associations).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnrichmentEdgeEndpointKind {
    #[default]
    Note,
    Proposal,
}

/// Typed implicit edge between two memory entities.
///
/// Note↔note edges populate `source_note_id` / `target_note_id`; note↔proposal
/// and proposal↔proposal edges populate `source_proposal_id` /
/// `target_proposal_id` and leave the note-id fields empty. `source_kind` /
/// `target_kind` are `None` (i.e. default to `Note`) for the legacy note↔note
/// path; they are explicit otherwise. Kinds: `builds_on` / `contradicts` /
/// `supersedes` / `exemplifies` / `derived_from` (provenance only).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct EnrichmentEdge {
    /// Source note id (note↔note substrate). Empty when the source is a proposal.
    #[serde(default)]
    pub source_note_id: String,
    /// Target note id (note↔note substrate). Empty when the target is a proposal.
    #[serde(default)]
    pub target_note_id: String,
    /// Source proposal id (heterogeneous note↔proposal / proposal↔proposal substrate).
    #[serde(default)]
    pub source_proposal_id: Option<String>,
    /// Target proposal id (heterogeneous note↔proposal / proposal↔proposal substrate).
    #[serde(default)]
    pub target_proposal_id: Option<String>,
    /// Source endpoint kind tag. `None` defaults to `Note`.
    #[serde(default)]
    pub source_kind: Option<EnrichmentEdgeEndpointKind>,
    /// Target endpoint kind tag. `None` defaults to `Note`.
    #[serde(default)]
    pub target_kind: Option<EnrichmentEdgeEndpointKind>,
    pub kind: String,
    /// Confidence in [0.0, 1.0].
    pub confidence: f64,
    #[serde(default)]
    pub evidence_quote: Option<String>,
}

/// Structured report returned by the enrichment pass.
///
/// Mirrors `djinn_agent::actors::slot::memory_enrichment::EnrichmentReport`
/// one-for-one. The server-side bridge converts between the two at the
/// implementation boundary so the MCP wire shape stays stable.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct EnrichmentReport {
    pub project_id: String,
    #[serde(default)]
    pub entities: Vec<EnrichmentEntity>,
    #[serde(default)]
    pub claims: Vec<EnrichmentClaim>,
    #[serde(default)]
    pub edges: Vec<EnrichmentEdge>,
    /// Non-fatal warnings — provider errors, parse failures, etc. The pass
    /// always succeeds; warnings are surfaced in the response, never
    /// propagated as blocking failures.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Number of source notes processed. Uses `i64` on the MCP wire to avoid
    /// nonstandard unsigned integer schema formats.
    pub notes_processed: i64,
    /// Number of source proposals processed (Wave 3 / rbsi). May be `0` when
    /// the project has no proposals or when proposal loading failed
    /// (best-effort).
    #[serde(default)]
    pub proposals_processed: i64,
    pub batches_sent: i64,
    pub entity_merges: i64,
    pub edges_dropped_wikilink_dup: i64,
    /// Number of candidate edges dropped because they had an unsupported
    /// endpoint (proposal id not in batch, unknown kind, malformed pairing,
    /// or no evidence quote). The pass never panics on these — they're
    /// surfaced as warnings on the report.
    #[serde(default)]
    pub edges_dropped_unsupported_endpoint: i64,
}

/// Status of the enrichment trigger call.
///
/// `Completed` — the pass finished before the call returned and the report is
/// embedded. `Queued` — the pass was scheduled on a background task and the
/// report will be emitted through the pass's finish log line.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnrichmentStatus {
    #[default]
    Completed,
    Queued,
}

/// Bridge trait for the memory enrichment trigger.
///
/// The implementation lives in `djinn-server`'s `mcp_bridge` and delegates to
/// `djinn_agent::actors::slot::memory_enrichment::run_memory_enrichment_with_db`.
///
/// # Contract
///
/// Implementations **must**:
/// - Run the pass asynchronously without blocking the caller on the LLM
///   provider (i.e. yield cooperatively — the test stubs prove this).
/// - Log an `INFO` line at start and finish.
/// - Never propagate provider / parse failures as blocking errors; surface
///   them as `warnings` on the returned report.
#[async_trait]
pub trait MemoryEnrichmentOps: Send + Sync {
    /// Trigger the enrichment pass for a project. The `project_id` is the
    /// resolved DB identifier (already past the slug → id translation done
    /// by the MCP tool's project resolver).
    async fn run_enrichment(&self, project_id: &str) -> Result<EnrichmentReport, String>;
}

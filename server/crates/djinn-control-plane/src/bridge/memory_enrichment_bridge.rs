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

/// Typed implicit edge between two notes (`builds_on` / `contradicts` /
/// `supersedes` / `exemplifies` / `derived_from`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct EnrichmentEdge {
    pub source_note_id: String,
    pub target_note_id: String,
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
    pub batches_sent: i64,
    pub entity_merges: i64,
    pub edges_dropped_wikilink_dup: i64,
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

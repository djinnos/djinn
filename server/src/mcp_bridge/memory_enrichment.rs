//! Bridge impl that wires `MemoryEnrichmentOps` to the agent-side
//! `djinn_agent::actors::slot::memory_enrichment` module.
//!
//! Why this lives in the server binary: the trait is defined in
//! `djinn-control-plane` (the consumer) and the algorithm lives in
//! `djinn-agent`. Both crates sit on the same level of the dependency
//! graph (`djinn-agent` depends on `djinn-control-plane`, not the other
//! way around), so neither can directly reference the other without a
//! trait. The server crate depends on both and is the natural place to
//! close the loop — see the parent `bridge/memory_enrichment_bridge.rs`
//! docstring for the design rationale.

use async_trait::async_trait;
use djinn_control_plane::bridge::{
    EnrichmentClaim, EnrichmentEdge, EnrichmentEdgeEndpointKind, EnrichmentEntity,
    EnrichmentReport, MemoryEnrichmentOps,
};

/// Adapter that delegates `MemoryEnrichmentOps::run_enrichment` to the
/// agent-side `run_memory_enrichment_with_db` entry point.
///
/// The agent's `EnrichmentReport` and the bridge's mirror types are kept
/// structurally identical on purpose so the conversion is a 1:1 field
/// copy. The conversion is intentionally mechanical — anything richer
/// would belong in the bridge trait itself, not here.
pub(super) struct MemoryEnrichmentBridge {
    db: djinn_db::Database,
}

impl MemoryEnrichmentBridge {
    pub(super) fn new(db: djinn_db::Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl MemoryEnrichmentOps for MemoryEnrichmentBridge {
    async fn run_enrichment(&self, project_id: &str) -> Result<EnrichmentReport, String> {
        let agent_report = djinn_agent::actors::slot::run_memory_enrichment_with_db(
            project_id,
            Some(self.db.clone()),
        )
        .await;
        Ok(convert_report(agent_report))
    }
}

/// Translate the agent's report type into the bridge's mirror type.
///
/// Lives here (not in `djinn-control-plane`) so the wire type is the
/// only surface the consumer needs to know about, and the agent's type
/// can evolve independently.
fn convert_report(input: djinn_agent::actors::slot::EnrichmentReport) -> EnrichmentReport {
    EnrichmentReport {
        project_id: input.project_id,
        entities: input.entities.into_iter().map(convert_entity).collect(),
        claims: input.claims.into_iter().map(convert_claim).collect(),
        edges: input.edges.into_iter().map(convert_edge).collect(),
        warnings: input.warnings,
        notes_processed: input.notes_processed as i64,
        proposals_processed: input.proposals_processed as i64,
        batches_sent: input.batches_sent as i64,
        entity_merges: input.entity_merges as i64,
        edges_dropped_wikilink_dup: input.edges_dropped_wikilink_dup as i64,
        edges_dropped_unsupported_endpoint: input.edges_dropped_unsupported_endpoint as i64,
    }
}

fn convert_entity(e: djinn_agent::actors::slot::EnrichmentEntity) -> EnrichmentEntity {
    EnrichmentEntity {
        canonical_name: e.canonical_name,
        aliases: e.aliases,
    }
}

fn convert_claim(c: djinn_agent::actors::slot::EnrichmentClaim) -> EnrichmentClaim {
    EnrichmentClaim {
        statement: c.statement,
        source_note_id: c.source_note_id,
        evidence_quote: c.evidence_quote,
    }
}

fn convert_edge(e: djinn_agent::actors::slot::EnrichmentEdge) -> EnrichmentEdge {
    EnrichmentEdge {
        source_note_id: e.source_note_id,
        target_note_id: e.target_note_id,
        source_proposal_id: e.source_proposal_id,
        target_proposal_id: e.target_proposal_id,
        source_kind: e.source_kind.map(convert_endpoint_kind),
        target_kind: e.target_kind.map(convert_endpoint_kind),
        kind: e.kind,
        confidence: e.confidence,
        evidence_quote: e.evidence_quote,
    }
}

fn convert_endpoint_kind(
    k: djinn_agent::actors::slot::EnrichmentEdgeEndpointKind,
) -> EnrichmentEdgeEndpointKind {
    match k {
        djinn_agent::actors::slot::EnrichmentEdgeEndpointKind::Note => {
            EnrichmentEdgeEndpointKind::Note
        }
        djinn_agent::actors::slot::EnrichmentEdgeEndpointKind::Proposal => {
            EnrichmentEdgeEndpointKind::Proposal
        }
    }
}

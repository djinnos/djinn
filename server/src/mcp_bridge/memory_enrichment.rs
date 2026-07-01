//! Bridge impl that wires `MemoryEnrichmentOps` to the canonical
//! `djinn_slot::memory_enrichment` module.
//!
//! Why this lives in the server binary: the trait is defined in
//! `djinn-control-plane` (the consumer) and the algorithm lives in
//! `djinn-slot`. The server crate depends on both `djinn-control-plane`
//! and `djinn-slot` and is the natural place to close the loop — see the
//! parent `bridge/memory_enrichment_bridge.rs` docstring for the design
//! rationale.

use async_trait::async_trait;
use djinn_control_plane::bridge::{
    EnrichmentClaim, EnrichmentEdge, EnrichmentEntity, EnrichmentReport, MemoryEnrichmentOps,
};

/// Adapter that delegates `MemoryEnrichmentOps::run_enrichment` to the
/// canonical `djinn_slot::run_memory_enrichment_with_db` entry point.
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
        let slot_report =
            djinn_slot::run_memory_enrichment_with_db(project_id, Some(self.db.clone())).await;
        Ok(convert_report(slot_report))
    }
}

/// Translate the canonical slot report type into the bridge's mirror type.
///
/// Lives here (not in `djinn-control-plane`) so the wire type is the
/// only surface the consumer needs to know about, and the agent's type
/// can evolve independently.
fn convert_report(input: djinn_slot::EnrichmentReport) -> EnrichmentReport {
    EnrichmentReport {
        project_id: input.project_id,
        entities: input.entities.into_iter().map(convert_entity).collect(),
        claims: input.claims.into_iter().map(convert_claim).collect(),
        edges: input.edges.into_iter().map(convert_edge).collect(),
        warnings: input.warnings,
        notes_processed: input.notes_processed as i64,
        batches_sent: input.batches_sent as i64,
        entity_merges: input.entity_merges as i64,
        edges_dropped_wikilink_dup: input.edges_dropped_wikilink_dup as i64,
    }
}

fn convert_entity(e: djinn_slot::EnrichmentEntity) -> EnrichmentEntity {
    EnrichmentEntity {
        canonical_name: e.canonical_name,
        aliases: e.aliases,
    }
}

fn convert_claim(c: djinn_slot::EnrichmentClaim) -> EnrichmentClaim {
    EnrichmentClaim {
        statement: c.statement,
        source_note_id: c.source_note_id,
        evidence_quote: c.evidence_quote,
    }
}

fn convert_edge(e: djinn_slot::EnrichmentEdge) -> EnrichmentEdge {
    EnrichmentEdge {
        source_note_id: e.source_note_id,
        target_note_id: e.target_note_id,
        kind: e.kind,
        confidence: e.confidence,
        evidence_quote: e.evidence_quote,
    }
}

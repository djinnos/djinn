//! Memory enrichment pass (stub with public types preserved).
use serde::{Deserialize, Serialize};

pub(crate) const ENTITY_DEDUP_COSINE_THRESHOLD: f64 = 0.92;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnrichmentEntity {
    pub canonical_name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub entity_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnrichmentEdge {
    pub source: String,
    pub target: String,
    pub edge_type: String,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default)]
    pub evidence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnrichmentClaim {
    pub subject: String,
    pub claim: String,
    #[serde(default)]
    pub confidence: f64,
}

#[derive(Debug, Clone, Default)]
pub struct EnrichmentReport {
    pub entities: Vec<EnrichmentEntity>,
    pub edges: Vec<EnrichmentEdge>,
    pub claims: Vec<EnrichmentClaim>,
    pub errors: Vec<String>,
}

/// Run memory enrichment for a project.
pub async fn run_memory_enrichment(
    project_id: &str,
    db: djinn_db::Database,
    event_bus: djinn_core::events::EventBus,
) -> EnrichmentReport {
    run_memory_enrichment_with_db(project_id, &db, &event_bus).await
}

pub async fn run_memory_enrichment_with_db(
    _project_id: &str,
    _db: &djinn_db::Database,
    _event_bus: &djinn_core::events::EventBus,
) -> EnrichmentReport {
    EnrichmentReport::default()
}

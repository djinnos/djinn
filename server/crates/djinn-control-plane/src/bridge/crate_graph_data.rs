use schemars::JsonSchema;
use serde::Serialize;

/// Request for the `crate_graph` bridge operation. Currently empty — the
/// crate graph is always the full workspace view; project context is supplied
/// via `ProjectCtx`.
#[derive(Debug, Clone, Default)]
pub struct CrateGraphRequest;

/// A single workspace crate node in the crate-level dependency graph.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CrateNodeEntry {
    pub name: String,
    pub manifest_path: String,
    pub loc: usize,
    pub node_count: usize,
    pub fan_in: f64,
    pub fan_out: f64,
    pub inbound_weight: f64,
    pub outbound_weight: f64,
}

/// An aggregated cross-crate edge: source crate → target crate.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CrateEdgeEntry {
    pub source: String,
    pub target: String,
    pub weight: f64,
    pub edge_count: usize,
}

/// Full crate-level graph returned by the `crate_graph` bridge operation.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CrateGraphResponse {
    pub crates: Vec<CrateNodeEntry>,
    pub edges: Vec<CrateEdgeEntry>,
    /// Present when the graph is empty (e.g. not a Rust workspace) to
    /// communicate the reason without returning an error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

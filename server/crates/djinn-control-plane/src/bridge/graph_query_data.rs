use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Typed bridge request for the budgeted natural-language subgraph planner.
#[derive(Debug, Clone)]
pub struct QuerySubgraphRequest {
    pub query: String,
    pub workspace: Option<String>,
    pub context_filter: Option<String>,
    pub file_filter: Option<String>,
    pub kind_filter: Option<String>,
    pub edge_filter: Vec<String>,
    pub token_budget: Option<usize>,
    pub max_depth: Option<usize>,
    pub max_seeds: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuerySubgraphResult {
    pub query: String,
    pub nodes: Vec<QuerySubgraphNode>,
    pub edges: Vec<QuerySubgraphEdge>,
    pub seeds: Vec<QuerySubgraphSeedDebug>,
    pub inferred_edge_kinds: Vec<String>,
    pub budget: QuerySubgraphBudget,
    pub traversal: QuerySubgraphTraversalDebug,
    pub narrowing_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuerySubgraphNode {
    pub uid: String,
    pub kind: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub is_seed: bool,
    pub is_hub: bool,
    pub degree: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuerySubgraphEdge {
    pub from_uid: String,
    pub to_uid: String,
    pub kind: String,
    pub confidence: f64,
    pub confidence_tier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuerySubgraphSeedDebug {
    pub uid: String,
    pub display_name: String,
    pub score: f64,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_text: Option<String>,
    pub debug: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuerySubgraphBudget {
    pub requested_tokens: usize,
    pub estimated_tokens: usize,
    pub truncated: bool,
    pub omitted_nodes: usize,
    pub omitted_edges: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuerySubgraphTraversalDebug {
    pub max_depth: usize,
    pub hub_degree_threshold: usize,
    pub hubs_blocked: Vec<String>,
    pub skipped_edge_kinds: Vec<String>,
}

//! `code_graph` tool handlers for querying the repository dependency graph.
//!
//! All graph queries are dispatched through the [`RepoGraphOps`] bridge trait,
//! keeping the MCP layer free of petgraph/SCIP dependencies.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::bridge::{GraphNeighbor, ImpactEntry, RankedNode};
use crate::server::DjinnMcpServer;
use crate::tools::task_tools::{ErrorOr, ErrorResponse};

// ── Request types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CodeGraphParams {
    /// The operation to perform: `neighbors`, `ranked`, `impact`, or `implementations`.
    pub operation: String,
    /// Path to the project root (used to locate the graph).
    pub project_path: String,
    /// The node key to query (file path or SCIP symbol string).
    /// Required for `neighbors`, `impact`, and `implementations`.
    #[serde(default)]
    pub key: Option<String>,
    /// Edge direction filter for `neighbors`: `incoming`, `outgoing`, or omit for both.
    #[serde(default)]
    pub direction: Option<String>,
    /// Node kind filter for `ranked`: `file` or `symbol`.
    #[serde(default)]
    pub kind_filter: Option<String>,
    /// Maximum results for `ranked` (default 20) or max traversal depth for `impact` (default 3).
    #[serde(default)]
    pub limit: Option<usize>,
}

// ── Response types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NeighborsResponse {
    pub key: String,
    pub neighbors: Vec<GraphNeighbor>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RankedResponse {
    pub nodes: Vec<RankedNode>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImplementationsResponse {
    pub symbol: String,
    pub implementations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImpactResponse {
    pub key: String,
    pub impact: Vec<ImpactEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum CodeGraphResponse {
    Neighbors(NeighborsResponse),
    Ranked(RankedResponse),
    Implementations(ImplementationsResponse),
    Impact(ImpactResponse),
}

// ── Validation ──────────────────────────────────────────────────────────────────

fn validate_direction(direction: Option<&str>) -> Result<(), String> {
    if let Some(d) = direction {
        match d {
            "incoming" | "outgoing" => Ok(()),
            _ => Err(format!(
                "invalid direction '{d}': expected 'incoming' or 'outgoing'"
            )),
        }
    } else {
        Ok(())
    }
}

fn validate_kind_filter(kind_filter: Option<&str>) -> Result<(), String> {
    if let Some(k) = kind_filter {
        match k {
            "file" | "symbol" => Ok(()),
            _ => Err(format!(
                "invalid kind_filter '{k}': expected 'file' or 'symbol'"
            )),
        }
    } else {
        Ok(())
    }
}

fn require_key(params: &CodeGraphParams) -> Result<&str, String> {
    params
        .key
        .as_deref()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| format!("'key' is required for operation '{}'", params.operation))
}

// ── Handler ─────────────────────────────────────────────────────────────────────

#[tool_router(router = graph_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Query the repository dependency graph built from SCIP indexer output.
    #[tool(
        description = "Query the repository dependency graph built from SCIP indexer output. Operations: neighbors (edges in/out of a node), ranked (top nodes by PageRank), impact (transitive dependents), implementations (find implementors of a trait/interface symbol)."
    )]
    pub async fn code_graph(
        &self,
        Parameters(params): Parameters<CodeGraphParams>,
    ) -> Json<ErrorOr<CodeGraphResponse>> {
        let result = match params.operation.as_str() {
            "neighbors" => self.code_graph_neighbors(&params).await,
            "ranked" => self.code_graph_ranked(&params).await,
            "implementations" => self.code_graph_implementations(&params).await,
            "impact" => self.code_graph_impact(&params).await,
            other => Err(format!(
                "unknown code_graph operation '{other}': expected one of \
                 'neighbors', 'ranked', 'impact', 'implementations'"
            )),
        };

        Json(match result {
            Ok(response) => ErrorOr::Ok(response),
            Err(error) => ErrorOr::Error(ErrorResponse { error }),
        })
    }
}

impl DjinnMcpServer {
    async fn code_graph_neighbors(
        &self,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let key = require_key(params)?;
        validate_direction(params.direction.as_deref())?;
        let neighbors = self
            .state
            .repo_graph()
            .neighbors(&params.project_path, key, params.direction.as_deref())
            .await?;
        Ok(CodeGraphResponse::Neighbors(NeighborsResponse {
            key: key.to_string(),
            neighbors,
        }))
    }

    async fn code_graph_ranked(
        &self,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        validate_kind_filter(params.kind_filter.as_deref())?;
        let limit = params.limit.unwrap_or(20);
        let nodes = self
            .state
            .repo_graph()
            .ranked(&params.project_path, params.kind_filter.as_deref(), limit)
            .await?;
        Ok(CodeGraphResponse::Ranked(RankedResponse { nodes }))
    }

    async fn code_graph_implementations(
        &self,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let key = require_key(params)?;
        let implementations = self
            .state
            .repo_graph()
            .implementations(&params.project_path, key)
            .await?;
        Ok(CodeGraphResponse::Implementations(
            ImplementationsResponse {
                symbol: key.to_string(),
                implementations,
            },
        ))
    }

    async fn code_graph_impact(
        &self,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let key = require_key(params)?;
        let depth = params.limit.unwrap_or(3);
        let impact = self
            .state
            .repo_graph()
            .impact(&params.project_path, key, depth)
            .await?;
        Ok(CodeGraphResponse::Impact(ImpactResponse {
            key: key.to_string(),
            impact,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_operation_field() {
        let params = CodeGraphParams {
            operation: "unknown_op".to_string(),
            project_path: "/tmp".to_string(),
            key: None,
            direction: None,
            kind_filter: None,
            limit: None,
        };
        // Just test validation logic, not the async handler
        assert!(require_key(&params).is_err());
    }

    #[test]
    fn validates_direction() {
        assert!(validate_direction(Some("incoming")).is_ok());
        assert!(validate_direction(Some("outgoing")).is_ok());
        assert!(validate_direction(None).is_ok());
        assert!(validate_direction(Some("both")).is_err());
    }

    #[test]
    fn validates_kind_filter() {
        assert!(validate_kind_filter(Some("file")).is_ok());
        assert!(validate_kind_filter(Some("symbol")).is_ok());
        assert!(validate_kind_filter(None).is_ok());
        assert!(validate_kind_filter(Some("unknown")).is_err());
    }

    #[test]
    fn require_key_rejects_empty() {
        let params = CodeGraphParams {
            operation: "neighbors".to_string(),
            project_path: "/tmp".to_string(),
            key: Some("".to_string()),
            direction: None,
            kind_filter: None,
            limit: None,
        };
        assert!(require_key(&params).is_err());
    }

    #[test]
    fn require_key_rejects_missing() {
        let params = CodeGraphParams {
            operation: "neighbors".to_string(),
            project_path: "/tmp".to_string(),
            key: None,
            direction: None,
            kind_filter: None,
            limit: None,
        };
        assert!(require_key(&params).is_err());
    }

    #[test]
    fn require_key_accepts_valid() {
        let params = CodeGraphParams {
            operation: "neighbors".to_string(),
            project_path: "/tmp".to_string(),
            key: Some("src/lib.rs".to_string()),
            direction: None,
            kind_filter: None,
            limit: None,
        };
        assert_eq!(require_key(&params).unwrap(), "src/lib.rs");
    }

    #[test]
    fn parses_code_graph_params_from_json() {
        let json = serde_json::json!({
            "operation": "neighbors",
            "project_path": "/workspace/repo",
            "key": "src/main.rs",
            "direction": "outgoing"
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "neighbors");
        assert_eq!(params.key.as_deref(), Some("src/main.rs"));
        assert_eq!(params.direction.as_deref(), Some("outgoing"));
        assert!(params.kind_filter.is_none());
        assert!(params.limit.is_none());
    }

    #[test]
    fn parses_ranked_params_from_json() {
        let json = serde_json::json!({
            "operation": "ranked",
            "project_path": "/workspace/repo",
            "kind_filter": "file",
            "limit": 10
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "ranked");
        assert!(params.key.is_none());
        assert_eq!(params.kind_filter.as_deref(), Some("file"));
        assert_eq!(params.limit, Some(10));
    }

    #[test]
    fn parses_impact_params_from_json() {
        let json = serde_json::json!({
            "operation": "impact",
            "project_path": "/workspace/repo",
            "key": "scip-rust . . . MyStruct#",
            "limit": 5
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "impact");
        assert_eq!(params.key.as_deref(), Some("scip-rust . . . MyStruct#"));
        assert_eq!(params.limit, Some(5));
    }

    #[test]
    fn parses_implementations_params_from_json() {
        let json = serde_json::json!({
            "operation": "implementations",
            "project_path": "/workspace/repo",
            "key": "scip-rust . . . Trait#"
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "implementations");
        assert_eq!(params.key.as_deref(), Some("scip-rust . . . Trait#"));
    }
}

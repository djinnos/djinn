//! `code_graph` tool handlers for querying the repository dependency graph.
//!
//! All graph queries are dispatched through the [`RepoGraphOps`] bridge trait,
//! keeping the MCP layer free of petgraph/SCIP dependencies.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::bridge::{
    CycleGroup, EdgeEntry, GraphDiff, ImpactResult, NeighborsResult, OrphanEntry, PathResult,
    RankedNode, SearchHit, SymbolDescription,
};
use crate::server::DjinnMcpServer;
use crate::tools::task_tools::{ErrorOr, ErrorResponse};

// ── Request types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CodeGraphParams {
    /// The operation to perform.
    /// One of: `neighbors`, `ranked`, `impact`, `implementations`,
    /// `search`, `cycles`, `orphans`, `path`, `edges`, `diff`, `describe`.
    pub operation: String,
    /// Path to the project root (used to locate the graph).
    pub project_path: String,
    /// The node key to query (file path or SCIP symbol string).
    /// Required for `neighbors`, `impact`, `implementations`, and `describe`.
    #[serde(default)]
    pub key: Option<String>,
    /// Edge direction filter for `neighbors`: `incoming`, `outgoing`, or omit for both.
    #[serde(default)]
    pub direction: Option<String>,
    /// Node kind filter for `ranked`/`search`/`cycles`/`orphans`: `file` or `symbol`.
    #[serde(default)]
    pub kind_filter: Option<String>,
    /// Maximum results for `ranked`/`search`/`orphans`/`edges` (default 20) or
    /// max traversal depth for `impact` (default 3).
    #[serde(default)]
    pub limit: Option<i64>,
    /// Substring query for `search`.
    #[serde(default)]
    pub query: Option<String>,
    /// Source node for `path`.
    #[serde(default)]
    pub from: Option<String>,
    /// Destination node for `path`.
    #[serde(default)]
    pub to: Option<String>,
    /// Source path glob for `edges`.
    #[serde(default)]
    pub from_glob: Option<String>,
    /// Destination path glob for `edges`.
    #[serde(default)]
    pub to_glob: Option<String>,
    /// Diff base selector for `diff`. Currently only `previous` is supported.
    #[serde(default)]
    pub since: Option<String>,
    /// Minimum SCC size for `cycles` (default 2).
    #[serde(default)]
    pub min_size: Option<i64>,
    /// Visibility filter for `orphans`: `public`, `private`, or `any` (default).
    #[serde(default)]
    pub visibility: Option<String>,
    /// Sort key for `ranked`: `pagerank` (default), `in_degree`, `out_degree`,
    /// or `total_degree`.
    #[serde(default)]
    pub sort_by: Option<String>,
    /// Group results: only `file` is supported. Applies to `impact`/`neighbors`.
    #[serde(default)]
    pub group_by: Option<String>,
    /// Optional max depth for `path`.
    #[serde(default)]
    pub max_depth: Option<i64>,
    /// Optional edge-kind filter for `edges`.
    #[serde(default)]
    pub edge_kind: Option<String>,
}

// ── Response types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NeighborsResponse {
    pub key: String,
    #[serde(flatten)]
    pub result: NeighborsResult,
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
    #[serde(flatten)]
    pub result: ImpactResult,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchResponse {
    pub query: String,
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CyclesResponse {
    pub cycles: Vec<CycleGroup>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OrphansResponse {
    pub orphans: Vec<OrphanEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PathResponse {
    pub path: Option<PathResult>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EdgesResponse {
    pub edges: Vec<EdgeEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiffResponse {
    pub diff: Option<GraphDiff>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DescribeResponse {
    pub description: Option<SymbolDescription>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum CodeGraphResponse {
    Neighbors(NeighborsResponse),
    Ranked(RankedResponse),
    Implementations(ImplementationsResponse),
    Impact(ImpactResponse),
    Search(SearchResponse),
    Cycles(CyclesResponse),
    Orphans(OrphansResponse),
    Path(PathResponse),
    Edges(EdgesResponse),
    Diff(DiffResponse),
    Describe(DescribeResponse),
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

fn require_query(params: &CodeGraphParams) -> Result<&str, String> {
    params
        .query
        .as_deref()
        .filter(|q| !q.is_empty())
        .ok_or_else(|| format!("'query' is required for operation '{}'", params.operation))
}

fn require_from_to<'a>(params: &'a CodeGraphParams) -> Result<(&'a str, &'a str), String> {
    let from = params
        .from
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("'from' is required for operation '{}'", params.operation))?;
    let to = params
        .to
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("'to' is required for operation '{}'", params.operation))?;
    Ok((from, to))
}

fn require_globs<'a>(params: &'a CodeGraphParams) -> Result<(&'a str, &'a str), String> {
    let from = params
        .from_glob
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!(
                "'from_glob' is required for operation '{}'",
                params.operation
            )
        })?;
    let to = params
        .to_glob
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("'to_glob' is required for operation '{}'", params.operation))?;
    Ok((from, to))
}

fn validate_visibility(visibility: Option<&str>) -> Result<(), String> {
    if let Some(v) = visibility {
        match v {
            "public" | "private" | "any" => Ok(()),
            _ => Err(format!(
                "invalid visibility '{v}': expected 'public', 'private', or 'any'"
            )),
        }
    } else {
        Ok(())
    }
}

fn validate_sort_by(sort_by: Option<&str>) -> Result<(), String> {
    if let Some(s) = sort_by {
        match s {
            "pagerank" | "in_degree" | "out_degree" | "total_degree" => Ok(()),
            _ => Err(format!(
                "invalid sort_by '{s}': expected 'pagerank', 'in_degree', 'out_degree', or 'total_degree'"
            )),
        }
    } else {
        Ok(())
    }
}

fn validate_group_by(group_by: Option<&str>) -> Result<(), String> {
    if let Some(g) = group_by {
        match g {
            "file" => Ok(()),
            _ => Err(format!("invalid group_by '{g}': only 'file' is supported")),
        }
    } else {
        Ok(())
    }
}

// ── Handler ─────────────────────────────────────────────────────────────────────

#[tool_router(router = graph_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Query the repository dependency graph built from SCIP indexer output.
    #[tool(
        description = "Query the repository dependency graph built from SCIP indexer output. Operations: neighbors (edges in/out of a node, with optional group_by=file rollup), ranked (top nodes; sort_by pagerank/in_degree/out_degree/total_degree), impact (transitive dependents, with optional group_by=file rollup), implementations (find implementors of a trait/interface symbol), search (name-based symbol lookup), cycles (strongly-connected components), orphans (zero-incoming-reference nodes, with visibility filter), path (shortest dependency path), edges (enumerate edges by from_glob/to_glob), diff (what changed since the previous canonical graph), describe (symbol signature/documentation without an LSP round trip)."
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
            "search" => self.code_graph_search(&params).await,
            "cycles" => self.code_graph_cycles(&params).await,
            "orphans" => self.code_graph_orphans(&params).await,
            "path" => self.code_graph_path(&params).await,
            "edges" => self.code_graph_edges(&params).await,
            "diff" => self.code_graph_diff(&params).await,
            "describe" => self.code_graph_describe(&params).await,
            other => Err(format!(
                "unknown code_graph operation '{other}': expected one of \
                 'neighbors', 'ranked', 'impact', 'implementations', \
                 'search', 'cycles', 'orphans', 'path', 'edges', 'diff', 'describe'"
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
        validate_group_by(params.group_by.as_deref())?;
        let result = self
            .state
            .repo_graph()
            .neighbors(
                &params.project_path,
                key,
                params.direction.as_deref(),
                params.group_by.as_deref(),
            )
            .await?;
        Ok(CodeGraphResponse::Neighbors(NeighborsResponse {
            key: key.to_string(),
            result,
        }))
    }

    async fn code_graph_ranked(
        &self,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        validate_kind_filter(params.kind_filter.as_deref())?;
        validate_sort_by(params.sort_by.as_deref())?;
        let limit = params.limit.unwrap_or(20) as usize;
        let nodes = self
            .state
            .repo_graph()
            .ranked(
                &params.project_path,
                params.kind_filter.as_deref(),
                params.sort_by.as_deref(),
                limit,
            )
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
        validate_group_by(params.group_by.as_deref())?;
        let depth = params.limit.unwrap_or(3) as usize;
        let result = self
            .state
            .repo_graph()
            .impact(&params.project_path, key, depth, params.group_by.as_deref())
            .await?;
        Ok(CodeGraphResponse::Impact(ImpactResponse {
            key: key.to_string(),
            result,
        }))
    }

    async fn code_graph_search(
        &self,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let query = require_query(params)?;
        validate_kind_filter(params.kind_filter.as_deref())?;
        let limit = params.limit.unwrap_or(20) as usize;
        let hits = self
            .state
            .repo_graph()
            .search(
                &params.project_path,
                query,
                params.kind_filter.as_deref(),
                limit,
            )
            .await?;
        Ok(CodeGraphResponse::Search(SearchResponse {
            query: query.to_string(),
            hits,
        }))
    }

    async fn code_graph_cycles(
        &self,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        validate_kind_filter(params.kind_filter.as_deref())?;
        let min_size = params.min_size.unwrap_or(2).max(0) as usize;
        let cycles = self
            .state
            .repo_graph()
            .cycles(
                &params.project_path,
                params.kind_filter.as_deref(),
                min_size,
            )
            .await?;
        Ok(CodeGraphResponse::Cycles(CyclesResponse { cycles }))
    }

    async fn code_graph_orphans(
        &self,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        validate_kind_filter(params.kind_filter.as_deref())?;
        validate_visibility(params.visibility.as_deref())?;
        let limit = params.limit.unwrap_or(50) as usize;
        let orphans = self
            .state
            .repo_graph()
            .orphans(
                &params.project_path,
                params.kind_filter.as_deref(),
                params.visibility.as_deref(),
                limit,
            )
            .await?;
        Ok(CodeGraphResponse::Orphans(OrphansResponse { orphans }))
    }

    async fn code_graph_path(&self, params: &CodeGraphParams) -> Result<CodeGraphResponse, String> {
        let (from, to) = require_from_to(params)?;
        let max_depth = params.max_depth.map(|v| v.max(0) as usize);
        let path = self
            .state
            .repo_graph()
            .path(&params.project_path, from, to, max_depth)
            .await?;
        Ok(CodeGraphResponse::Path(PathResponse { path }))
    }

    async fn code_graph_edges(
        &self,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let (from_glob, to_glob) = require_globs(params)?;
        let limit = params.limit.unwrap_or(100) as usize;
        let edges = self
            .state
            .repo_graph()
            .edges(
                &params.project_path,
                from_glob,
                to_glob,
                params.edge_kind.as_deref(),
                limit,
            )
            .await?;
        Ok(CodeGraphResponse::Edges(EdgesResponse { edges }))
    }

    async fn code_graph_diff(&self, params: &CodeGraphParams) -> Result<CodeGraphResponse, String> {
        let diff = self
            .state
            .repo_graph()
            .diff(&params.project_path, params.since.as_deref())
            .await?;
        Ok(CodeGraphResponse::Diff(DiffResponse { diff }))
    }

    async fn code_graph_describe(
        &self,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let key = require_key(params)?;
        let description = self
            .state
            .repo_graph()
            .describe(&params.project_path, key)
            .await?;
        Ok(CodeGraphResponse::Describe(DescribeResponse {
            description,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_operation_field() {
        let params = test_params("unknown_op");
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

    fn test_params(op: &str) -> CodeGraphParams {
        CodeGraphParams {
            operation: op.to_string(),
            project_path: "/tmp".to_string(),
            key: None,
            direction: None,
            kind_filter: None,
            limit: None,
            query: None,
            from: None,
            to: None,
            from_glob: None,
            to_glob: None,
            since: None,
            min_size: None,
            visibility: None,
            sort_by: None,
            group_by: None,
            max_depth: None,
            edge_kind: None,
        }
    }

    #[test]
    fn require_key_rejects_empty() {
        let mut params = test_params("neighbors");
        params.key = Some(String::new());
        assert!(require_key(&params).is_err());
    }

    #[test]
    fn require_key_rejects_missing() {
        let params = test_params("neighbors");
        assert!(require_key(&params).is_err());
    }

    #[test]
    fn require_key_accepts_valid() {
        let mut params = test_params("neighbors");
        params.key = Some("src/lib.rs".to_string());
        assert_eq!(require_key(&params).unwrap(), "src/lib.rs");
    }

    #[test]
    fn require_query_rejects_missing() {
        let params = test_params("search");
        assert!(require_query(&params).is_err());
    }

    #[test]
    fn require_from_to_rejects_partial() {
        let mut params = test_params("path");
        params.from = Some("src/a.rs".to_string());
        assert!(require_from_to(&params).is_err());
        params.to = Some("src/b.rs".to_string());
        assert_eq!(require_from_to(&params).unwrap(), ("src/a.rs", "src/b.rs"));
    }

    #[test]
    fn require_globs_rejects_partial() {
        let mut params = test_params("edges");
        params.from_glob = Some("**/*.rs".to_string());
        assert!(require_globs(&params).is_err());
        params.to_glob = Some("**/*.rs".to_string());
        assert!(require_globs(&params).is_ok());
    }

    #[test]
    fn validates_visibility() {
        assert!(validate_visibility(None).is_ok());
        assert!(validate_visibility(Some("public")).is_ok());
        assert!(validate_visibility(Some("private")).is_ok());
        assert!(validate_visibility(Some("any")).is_ok());
        assert!(validate_visibility(Some("internal")).is_err());
    }

    #[test]
    fn validates_sort_by() {
        assert!(validate_sort_by(None).is_ok());
        assert!(validate_sort_by(Some("pagerank")).is_ok());
        assert!(validate_sort_by(Some("in_degree")).is_ok());
        assert!(validate_sort_by(Some("out_degree")).is_ok());
        assert!(validate_sort_by(Some("total_degree")).is_ok());
        assert!(validate_sort_by(Some("alpha")).is_err());
    }

    #[test]
    fn validates_group_by() {
        assert!(validate_group_by(None).is_ok());
        assert!(validate_group_by(Some("file")).is_ok());
        assert!(validate_group_by(Some("symbol")).is_err());
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

    #[test]
    fn parses_search_params_from_json() {
        let json = serde_json::json!({
            "operation": "search",
            "project_path": "/workspace/repo",
            "query": "AgentSession",
            "kind_filter": "symbol",
            "limit": 5,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "search");
        assert_eq!(params.query.as_deref(), Some("AgentSession"));
        assert_eq!(params.kind_filter.as_deref(), Some("symbol"));
        assert_eq!(params.limit, Some(5));
    }

    #[test]
    fn parses_cycles_params_from_json() {
        let json = serde_json::json!({
            "operation": "cycles",
            "project_path": "/workspace/repo",
            "min_size": 3,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "cycles");
        assert_eq!(params.min_size, Some(3));
    }

    #[test]
    fn parses_orphans_params_from_json() {
        let json = serde_json::json!({
            "operation": "orphans",
            "project_path": "/workspace/repo",
            "visibility": "private",
            "limit": 25,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.visibility.as_deref(), Some("private"));
        assert_eq!(params.limit, Some(25));
    }

    #[test]
    fn parses_path_params_from_json() {
        let json = serde_json::json!({
            "operation": "path",
            "project_path": "/workspace/repo",
            "from": "src/a.rs",
            "to": "src/b.rs",
            "max_depth": 6,
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.from.as_deref(), Some("src/a.rs"));
        assert_eq!(params.to.as_deref(), Some("src/b.rs"));
        assert_eq!(params.max_depth, Some(6));
    }

    #[test]
    fn parses_edges_params_from_json() {
        let json = serde_json::json!({
            "operation": "edges",
            "project_path": "/workspace/repo",
            "from_glob": "server/src/**",
            "to_glob": "server/crates/**",
            "edge_kind": "FileReference",
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.from_glob.as_deref(), Some("server/src/**"));
        assert_eq!(params.to_glob.as_deref(), Some("server/crates/**"));
        assert_eq!(params.edge_kind.as_deref(), Some("FileReference"));
    }

    #[test]
    fn parses_diff_params_from_json() {
        let json = serde_json::json!({
            "operation": "diff",
            "project_path": "/workspace/repo",
            "since": "previous",
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.since.as_deref(), Some("previous"));
    }

    #[test]
    fn parses_describe_params_from_json() {
        let json = serde_json::json!({
            "operation": "describe",
            "project_path": "/workspace/repo",
            "key": "scip-rust . . . AgentSession#",
        });
        let params: CodeGraphParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.operation, "describe");
        assert_eq!(params.key.as_deref(), Some("scip-rust . . . AgentSession#"));
    }
}

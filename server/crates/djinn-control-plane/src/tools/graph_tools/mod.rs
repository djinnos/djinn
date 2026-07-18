//! `code_graph` tool handlers for querying the repository dependency graph.
//!
//! All graph queries are dispatched through the [`RepoGraphOps`] bridge trait,
//! keeping the MCP layer free of petgraph/SCIP dependencies.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::Instrument;

use crate::bridge::{
    ApiImpactResult, ApiSurfaceEntry, BoundaryRule, BoundaryViolation, Candidate, ChangedRange,
    ChurnEntry, ComplexityResult, CoupledPairEntry, CouplingEntry, CouplingHubEntry,
    CrateEdgeEntry, CrateGraphResponse, CrateNodeEntry, CycleGroup, DeadSymbolEntry, DeprecatedHit,
    DetectedChangesResult, EdgeEntry, FileGroupEntry, FlowResult, GraphNeighbor, GraphStatus,
    HotPathHit, HotspotEntry, ImpactEntry, ImpactResult, MetricsAtResult, NeighborsResult,
    OrphanEntry, PathResult, ProjectCtx, QuerySubgraphRequest, QuerySubgraphResult, RankedNode,
    RefactorCandidate, ResolveOutcome, RouteMapResult, SearchHit, ShapeCheckResult, SnapshotLevel,
    SnapshotPayload, SymbolAtHit, SymbolContext, SymbolDescription, TouchedSymbol,
    WorkspacesResult,
};
use crate::server::DjinnMcpServer;
use crate::tools::graph_exclusions::GraphExclusions;
use crate::tools::task_tools::{ErrorOr, ErrorResponse};
use djinn_db::ProjectRepository;

/// Maximum node-selection budget accepted by MCP `code_graph snapshot`.
pub const MAX_SNAPSHOT_NODE_CAP: usize = 10_000;

mod handler;
mod handler_basic_ops;
mod handler_change_ops;
mod handler_coupling_ops;
mod handler_impact_check;
mod next_step_hints;
pub mod operation_registry;
mod request_types;
mod response_types;
mod risk_classification;
#[cfg(test)]
mod tests;
mod validation;
// df6s: shared pagination slicing/counting helpers, plus the
// `PaginationParams` summary struct used by the agent-boundary
// pagination work on `neighbors` / `impact` / `coupling_hotspots`.
mod df6s_pagination;

pub(crate) use self::next_step_hints::*;
pub(crate) use self::response_types::check_impact_staleness;
pub(crate) use self::risk_classification::*;
pub(crate) use self::validation::*;

pub(crate) use self::df6s_pagination::{
    apply_page_slice, build_by_depth_counts, pagination_applied,
};
pub(crate) use self::request_types::PaginationParams;
pub use self::request_types::{CodeGraphParams, TestFilter};
pub use self::response_types::*;
pub(crate) use self::validation::{SearchMode, resolve_search_mode};

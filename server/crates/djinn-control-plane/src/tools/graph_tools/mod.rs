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
    ApiSurfaceEntry, BoundaryRule, BoundaryViolation, Candidate, ChangedRange, ChurnEntry,
    ComplexityResult, CoupledPairEntry, CouplingEntry, CouplingHubEntry, CycleGroup,
    DeadSymbolEntry, DeprecatedHit, DetectedChangesResult, EdgeEntry, FileGroupEntry,
    GraphNeighbor, GraphStatus, HotPathHit, HotspotEntry, ImpactEntry, ImpactResult,
    MetricsAtResult, NeighborsResult, OrphanEntry, PathResult, ProjectCtx, QuerySubgraphRequest,
    QuerySubgraphResult, RankedNode, RefactorCandidate, ResolveOutcome, SearchHit, SnapshotLevel,
    SnapshotPayload, SymbolAtHit, SymbolContext, SymbolDescription, TouchedSymbol,
    WorkspacesResult,
};
use crate::server::DjinnMcpServer;
use crate::tools::graph_exclusions::GraphExclusions;
use crate::tools::task_tools::{ErrorOr, ErrorResponse};
use djinn_db::ProjectRepository;

mod handler;
mod handler_basic_ops;
mod handler_change_ops;
mod handler_coupling_ops;
mod next_step_hints;
mod request_types;
mod response_types;
mod risk_classification;
#[cfg(test)]
mod tests;
mod validation;

use self::next_step_hints::*;
use self::risk_classification::*;
use self::validation::*;

pub use self::request_types::{CodeGraphParams, TestFilter};
pub use self::response_types::*;
pub(crate) use self::validation::{SearchMode, resolve_search_mode};

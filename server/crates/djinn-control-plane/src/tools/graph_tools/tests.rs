//! Tests for the `graph_tools` concern.
//!
//! The test bodies live in `tests/` as real modules. They used to be `.inc`
//! fragments pulled in with the textual include macro, which made them
//! invisible to the code graph: `rust-analyzer scip` emits no document for an
//! included file, so nothing defined inside one could be found by an agent,
//! and the `*.rs`-filtered CI guards skipped them too. The macro is now
//! banned outright — see `scripts/check-include-macro.sh`.
//!
//! Imports and fixtures shared by more than one child module are declared
//! here — the children reach them through `use super::*`, exactly as the
//! single flat module did before the split, without widening any real
//! visibility.

#![allow(unused_imports)]

use super::handler_impact_check::{
    CrateIndex, ImpactAggregator, check_impact_check_staleness,
    derive_safe_slice_and_recommendation,
};
use super::*;
use crate::bridge::*;
use crate::tools::graph_exclusions::GraphExclusions;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use djinn_core::events::EventBus;
use djinn_db::Database;
use djinn_provider::catalog::{CatalogService, HealthTracker};

use crate::bridge::{
    ApiSurfaceEntry, BoundaryRule, BoundaryViolation, ChangedRange, ChurnEntry, ComplexityResult,
    CoupledPairEntry, CouplingEntry, CouplingHubEntry, CrateEdgeEntry, CrateGraphResponse,
    CrateNodeEntry, CycleGroup, DeadSymbolEntry, DeprecatedHit, DetectedChangesResult,
    DiffTouchesResult, EdgeEntry, GraphNeighbor, GraphStatus, HotPathHit, HotspotEntry,
    ImpactResult, MetricsAtResult, NeighborsResult, OrphanEntry, PathResult, ProjectCtx,
    QuerySubgraphBudget, QuerySubgraphEdge, QuerySubgraphNode, QuerySubgraphRequest,
    QuerySubgraphResult, QuerySubgraphSeedDebug, QuerySubgraphTraversalDebug, RankedNode,
    RefactorCandidate, RepoGraphOps, ResolveOutcome, SearchHit, SnapshotPayload, SymbolAtHit,
    SymbolContext, SymbolDescription,
};
use crate::server::DjinnMcpServer;
use crate::state::McpState;

mod tests_coverage;
mod tests_crate_graph;
mod tests_df6s_pagination;
mod tests_part1;
mod tests_part2;
mod tests_part3;
mod tests_registry_dispatch;

// Fixtures shared across the child modules. Re-exporting them here — rather
// than having each child import from each sibling — keeps the shared surface
// in one greppable place, and `use super::*` in a child picks them up.
use tests_part1::WorkspaceFixtureOps;
use tests_part2::{fixture_ctx, fixture_server, test_params};

/// Bridge traits that decouple djinn-control-plane from the server binary.
///
/// The server implements each trait for its concrete handle types
/// (CoordinatorHandle, SlotPoolHandle, LspManager, AppState).
/// McpState holds Arc<dyn Trait> so the MCP layer never imports server types.
mod coordinator_bridge;
mod extension_diagnostics_probe_bridge;
mod git_bridge;
mod graph_bridge;
mod graph_data;
mod graph_query_data;
mod lsp_bridge;
mod memory_enrichment_bridge;
mod runtime_bridge;
mod slot_pool_bridge;

pub use self::coordinator_bridge::{
    CoordinatorOps, CoordinatorStatus, ProposalRefinementStartRequest,
};
pub use self::extension_diagnostics_probe_bridge::ExtensionDiagnosticsProbeOps;
pub use self::git_bridge::GitOps;
pub use self::graph_bridge::{REPO_GRAPH_OPS_METHODS, RepoGraphOps};
pub use self::graph_data::{
    ApiImpactEntry, ApiImpactResult, ApiSurfaceEntry, BoundaryRule, BoundaryViolation, CallerRef,
    Candidate, ChangeKind, ChangedRange, ChurnEntry, ComplexityMetrics, ComplexityResult,
    CoupledPairEntry, CouplingEntry, CouplingHubEntry, CrateEdgeEntry, CrateGraphRequest,
    CrateGraphResponse, CrateNodeEntry, CycleGroup, CycleMember, DeadSymbolEntry, DeprecatedHit,
    DetectedChangesResult, DetectedTouchedSymbol, DiffTouchesResult, EdgeCategory, EdgeEntry,
    FileComplexityEntry, FileGroupEntry, FlowHit, FlowResult, FunctionComplexityEntry,
    GraphNeighbor, GraphStatus, GraphWorkspaceEntry, HotPathHit, HotspotEntry, ImpactEntry,
    ImpactResult, MethodMeta, MethodParam, MetricsAtResult, NeighborsResult, OrphanEntry,
    PagerankTier, PathHop, PathResult, ProcessRef, ProjectCtx, RankedNode, RefactorCandidate,
    RelatedSymbol, ResolveOutcome, RouteLanguageChain, RouteMapEntry, RouteMapResult, RouteRef,
    RouteShape, RouteSummary, SearchHit, SemanticQueryEmbedding, ShapeCheckResult, ShapeDrift,
    ShapeField, ShapeTypeMismatch, SnapshotEdge, SnapshotLevel, SnapshotNode, SnapshotPayload,
    SymbolAtHit, SymbolContext, SymbolDescription, SymbolNode, TouchedSymbol, WorkspaceScope,
    WorkspacesResult,
};
pub use self::graph_query_data::{
    QuerySubgraphBudget, QuerySubgraphEdge, QuerySubgraphNode, QuerySubgraphRequest,
    QuerySubgraphResult, QuerySubgraphSeedDebug, QuerySubgraphTraversalDebug,
};
pub use self::lsp_bridge::{LspOps, LspWarning};
pub use self::memory_enrichment_bridge::{
    EnrichmentClaim, EnrichmentEdge, EnrichmentEntity, EnrichmentReport, EnrichmentStatus,
    MemoryEnrichmentOps,
};
pub use self::runtime_bridge::{RuntimeDispatchError, RuntimeOps, TaskrunJobRef};
pub use self::slot_pool_bridge::{ModelPoolStatus, PoolStatus, RunningTaskInfo, SlotPoolOps};

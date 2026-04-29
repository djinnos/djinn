//! In-crate test harness for MCP tool handlers.
//!
//! Gated behind `#[cfg(any(test, feature = "test-support"))]` so downstream
//! integration tests (inside this crate's `tests/` directory, or any workspace
//! crate that enables `djinn-control-plane/test-support`) can stand up a
//! `McpTestHarness`, dispatch tools by name with JSON args, and assert on
//! results without wiring up `AppState`, the Axum server, or live actors.
//!
//! ## Stub policy
//!
//! Each bridge trait (`CoordinatorOps`, `SlotPoolOps`, `LspOps`, `RuntimeOps`,
//! `GitOps`, `RepoGraphOps`) and provider trait (`NoteEmbeddingProvider`,
//! `NoteVectorStore`) has a harness-local stub.  Stubs return:
//!
//! * **Query methods** — sensible empties (`None`, `Vec::new()`, `Ok(None)`),
//!   plus `Ok(false)` for existence probes and default `GraphStatus` for the
//!   canonical-graph cache peek.  A test that happens to route through a
//!   query still gets a clean "nothing found" answer rather than panicking.
//! * **Mutate methods** — a visible error of the form
//!   `"stub: <TraitName>::<method> not implemented"` so tests that
//!   accidentally touch a stubbed mutation get a clear failure, not a silent
//!   no-op.
//!
//! This is deliberately stricter than the legacy `state::stubs` module, which
//! has permissive `Ok` returns for a handful of `RuntimeOps` mutations that
//! pre-existing unit tests rely on.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use djinn_core::events::EventBus;
use djinn_core::models::DjinnSettings;
use djinn_db::{
    Database,
    error::{DbError, DbResult},
    repositories::note::{
        EmbeddedNote, EmbeddingQueryContext, NoteEmbeddingMatch, NoteEmbeddingProvider,
        NoteEmbeddingRecord, NoteRepository, NoteVectorBackend, NoteVectorStore,
        UpsertNoteEmbedding,
    },
};
use djinn_git::{GitActorHandle, GitError};
use djinn_provider::catalog::{CatalogService, HealthTracker};
use serde_json::Value;

use crate::bridge::{
    ApiSurfaceEntry, BoundaryRule, BoundaryViolation, ChangedRange, CoordinatorOps,
    CoordinatorStatus, CycleGroup, DeadSymbolEntry, DeprecatedHit, DiffTouchesResult, EdgeEntry,
    GitOps, GraphStatus, HotPathHit, HotspotEntry, ImpactResult, LspOps, LspWarning,
    MetricsAtResult, NeighborsResult, OrphanEntry, PathResult, PoolStatus, ProjectCtx, RankedNode,
    RepoGraphOps, RunningTaskInfo, RuntimeOps, SearchHit, SemanticQueryEmbedding, SlotPoolOps,
    SymbolAtHit, SymbolDescription,
};
use crate::server::DjinnMcpServer;
use crate::state::McpState;

// ── Strict stubs ───────────────────────────────────────────────────────────────

/// CoordinatorOps stub. `get_status` returns a visible error (it's the
/// *reading* side of a subsystem we haven't wired up — not "no data");
/// `trigger_dispatch_for_project` is a mutation and also errors.
pub struct StubCoordinator;

#[async_trait]
impl CoordinatorOps for StubCoordinator {
    fn get_status(&self) -> std::result::Result<CoordinatorStatus, String> {
        Err("stub: CoordinatorOps::get_status not implemented".into())
    }
    async fn trigger_dispatch_for_project(
        &self,
        _project_id: &str,
    ) -> std::result::Result<(), String> {
        Err("stub: CoordinatorOps::trigger_dispatch_for_project not implemented".into())
    }
}

/// SlotPoolOps stub. Queries return empties; mutations (kill_session) error.
pub struct StubSlotPool;

#[async_trait]
impl SlotPoolOps for StubSlotPool {
    async fn get_status(&self) -> std::result::Result<PoolStatus, String> {
        Ok(PoolStatus {
            active_slots: 0,
            total_slots: 0,
            per_model: Default::default(),
            running_tasks: Vec::new(),
        })
    }
    async fn kill_session(&self, _task_id: &str) -> std::result::Result<(), String> {
        Err("stub: SlotPoolOps::kill_session not implemented".into())
    }
    async fn session_for_task(
        &self,
        _task_id: &str,
    ) -> std::result::Result<Option<RunningTaskInfo>, String> {
        Ok(None)
    }
    async fn has_session(&self, _task_id: &str) -> std::result::Result<bool, String> {
        Ok(false)
    }
}

/// LspOps stub. All calls are queries; returns no warnings.
pub struct StubLsp;

#[async_trait]
impl LspOps for StubLsp {
    async fn warnings(&self) -> Vec<LspWarning> {
        Vec::new()
    }
}

/// RuntimeOps stub. Every method here is a *mutation* (or side-effect), so
/// every fallible method errors loudly; the infallible ones (`reset_*`,
/// `persist_*`) are simple no-ops because their signature precludes signalling
/// failure.
pub struct StubRuntime;

#[async_trait]
impl RuntimeOps for StubRuntime {
    async fn apply_settings(
        &self,
        _settings: &DjinnSettings,
    ) -> std::result::Result<(), String> {
        Err("stub: RuntimeOps::apply_settings not implemented".into())
    }
    async fn embed_memory_query(
        &self,
        _query: &str,
    ) -> std::result::Result<Option<SemanticQueryEmbedding>, String> {
        Ok(None)
    }
    async fn reset_runtime_settings(&self) {}
    async fn persist_model_health_state(&self) {}
    async fn apply_environment_config(
        &self,
        _project_id: &str,
        _config: &djinn_stack::environment::EnvironmentConfig,
    ) -> std::result::Result<(), String> {
        Err("stub: RuntimeOps::apply_environment_config not implemented".into())
    }
}

/// GitOps stub. `git_actor` is effectively a read that creates a handle; it
/// returns a structured `GitError` so downstream code paths see a clean
/// "not a repo" signal rather than a stub panic.
pub struct StubGit;

#[async_trait]
impl GitOps for StubGit {
    async fn git_actor(&self, _path: &Path) -> std::result::Result<GitActorHandle, GitError> {
        Err(GitError::CommandFailed {
            code: 1,
            command: "stub-git-actor".into(),
            cwd: ".".into(),
            stdout: String::new(),
            stderr: "stub: GitOps::git_actor not implemented".into(),
        })
    }
}

/// RepoGraphOps stub. Every method is a query — return empties.
pub struct StubRepoGraph;

#[async_trait]
impl RepoGraphOps for StubRepoGraph {
    async fn neighbors(
        &self,
        _ctx: &ProjectCtx,
        _key: &str,
        _direction: Option<&str>,
        _group_by: Option<&str>,
        _kind_filter: Option<&str>,
    ) -> std::result::Result<NeighborsResult, String> {
        Ok(NeighborsResult::Detailed(Vec::new()))
    }
    async fn ranked(
        &self,
        _ctx: &ProjectCtx,
        _kind_filter: Option<&str>,
        _sort_by: Option<&str>,
        _limit: usize,
    ) -> std::result::Result<Vec<RankedNode>, String> {
        Ok(Vec::new())
    }
    async fn implementations(
        &self,
        _ctx: &ProjectCtx,
        _symbol: &str,
    ) -> std::result::Result<Vec<String>, String> {
        Ok(Vec::new())
    }
    async fn impact(
        &self,
        _ctx: &ProjectCtx,
        _key: &str,
        _depth: usize,
        _group_by: Option<&str>,
        _min_confidence: Option<f64>,
    ) -> std::result::Result<ImpactResult, String> {
        Ok(ImpactResult::Detailed(Vec::new()))
    }
    async fn search(
        &self,
        _ctx: &ProjectCtx,
        _query: &str,
        _kind_filter: Option<&str>,
        _limit: usize,
    ) -> std::result::Result<Vec<SearchHit>, String> {
        Ok(Vec::new())
    }
    async fn cycles(
        &self,
        _ctx: &ProjectCtx,
        _kind_filter: Option<&str>,
        _min_size: usize,
    ) -> std::result::Result<Vec<CycleGroup>, String> {
        Ok(Vec::new())
    }
    async fn orphans(
        &self,
        _ctx: &ProjectCtx,
        _kind_filter: Option<&str>,
        _visibility: Option<&str>,
        _limit: usize,
    ) -> std::result::Result<Vec<OrphanEntry>, String> {
        Ok(Vec::new())
    }
    async fn path(
        &self,
        _ctx: &ProjectCtx,
        _from: &str,
        _to: &str,
        _max_depth: Option<usize>,
    ) -> std::result::Result<Option<PathResult>, String> {
        Ok(None)
    }
    async fn edges(
        &self,
        _ctx: &ProjectCtx,
        _from_glob: &str,
        _to_glob: &str,
        _edge_kind: Option<&str>,
        _limit: usize,
    ) -> std::result::Result<Vec<EdgeEntry>, String> {
        Ok(Vec::new())
    }
    async fn describe(
        &self,
        _ctx: &ProjectCtx,
        _key: &str,
    ) -> std::result::Result<Option<SymbolDescription>, String> {
        Ok(None)
    }
    async fn context(
        &self,
        _ctx: &ProjectCtx,
        _key: &str,
        _include_content: bool,
    ) -> std::result::Result<Option<crate::bridge::SymbolContext>, String> {
        Ok(None)
    }
    async fn status(
        &self,
        ctx: &ProjectCtx,
    ) -> std::result::Result<GraphStatus, String> {
        Ok(GraphStatus {
            project_id: ctx.id.clone(),
            warmed: false,
            last_warm_at: None,
            pinned_commit: None,
            commits_since_pin: None,
        })
    }
    async fn snapshot(
        &self,
        ctx: &ProjectCtx,
        node_cap: usize,
        _exclusions: &crate::tools::graph_exclusions::GraphExclusions,
    ) -> std::result::Result<crate::bridge::SnapshotPayload, String> {
        Ok(crate::bridge::SnapshotPayload {
            project_id: ctx.id.clone(),
            git_head: String::new(),
            generated_at: String::new(),
            truncated: false,
            total_nodes: 0,
            total_edges: 0,
            node_cap,
            nodes: Vec::new(),
            edges: Vec::new(),
        })
    }
    async fn symbols_at(
        &self,
        _ctx: &ProjectCtx,
        _file: &str,
        _start_line: u32,
        _end_line: Option<u32>,
    ) -> std::result::Result<Vec<SymbolAtHit>, String> {
        Ok(Vec::new())
    }
    async fn diff_touches(
        &self,
        _ctx: &ProjectCtx,
        _changed_ranges: &[ChangedRange],
    ) -> std::result::Result<DiffTouchesResult, String> {
        Ok(DiffTouchesResult {
            touched_symbols: Vec::new(),
            affected_files: Vec::new(),
            unknown_files: Vec::new(),
        })
    }
    async fn detect_changes(
        &self,
        _ctx: &ProjectCtx,
        from_sha: Option<&str>,
        to_sha: Option<&str>,
        _changed_files: &[String],
    ) -> std::result::Result<crate::bridge::DetectedChangesResult, String> {
        Ok(crate::bridge::DetectedChangesResult {
            from_sha: from_sha.unwrap_or("").to_string(),
            to_sha: to_sha.unwrap_or("").to_string(),
            touched_symbols: Vec::new(),
            by_file: std::collections::BTreeMap::new(),
        })
    }
    async fn api_surface(
        &self,
        _ctx: &ProjectCtx,
        _module_glob: Option<&str>,
        _visibility: Option<&str>,
        _limit: usize,
    ) -> std::result::Result<Vec<ApiSurfaceEntry>, String> {
        Ok(Vec::new())
    }
    async fn boundary_check(
        &self,
        _ctx: &ProjectCtx,
        _rules: &[BoundaryRule],
    ) -> std::result::Result<Vec<BoundaryViolation>, String> {
        Ok(Vec::new())
    }
    async fn hotspots(
        &self,
        _ctx: &ProjectCtx,
        _window_days: u32,
        _file_glob: Option<&str>,
        _limit: usize,
    ) -> std::result::Result<Vec<HotspotEntry>, String> {
        Ok(Vec::new())
    }
    async fn metrics_at(
        &self,
        _ctx: &ProjectCtx,
    ) -> std::result::Result<MetricsAtResult, String> {
        Ok(MetricsAtResult {
            commit: String::new(),
            node_count: 0,
            edge_count: 0,
            cycle_count: 0,
            cycle_count_symbol_only: 0,
            cycle_count_file_only: 0,
            cycles_by_size_histogram: std::collections::BTreeMap::new(),
            god_object_count: 0,
            orphan_count: 0,
            public_api_count: 0,
            doc_coverage_pct: 0.0,
        })
    }
    async fn dead_symbols(
        &self,
        _ctx: &ProjectCtx,
        _confidence: &str,
        _limit: usize,
    ) -> std::result::Result<Vec<DeadSymbolEntry>, String> {
        Ok(Vec::new())
    }
    async fn deprecated_callers(
        &self,
        _ctx: &ProjectCtx,
        _limit: usize,
    ) -> std::result::Result<Vec<DeprecatedHit>, String> {
        Ok(Vec::new())
    }
    async fn touches_hot_path(
        &self,
        _ctx: &ProjectCtx,
        _seed_entries: &[String],
        _seed_sinks: &[String],
        _symbols: &[String],
    ) -> std::result::Result<Vec<HotPathHit>, String> {
        Ok(Vec::new())
    }
    async fn coupling(
        &self,
        _ctx: &ProjectCtx,
        _file_path: &str,
        _limit: usize,
    ) -> std::result::Result<Vec<crate::bridge::CouplingEntry>, String> {
        Ok(Vec::new())
    }
    async fn churn(
        &self,
        _ctx: &ProjectCtx,
        _limit: usize,
        _since_days: Option<u32>,
    ) -> std::result::Result<Vec<crate::bridge::ChurnEntry>, String> {
        Ok(Vec::new())
    }
    async fn coupling_hotspots(
        &self,
        _ctx: &ProjectCtx,
        _limit: usize,
        _since_days: Option<u32>,
        _max_files_per_commit: usize,
    ) -> std::result::Result<Vec<crate::bridge::CoupledPairEntry>, String> {
        Ok(Vec::new())
    }
    async fn coupling_hubs(
        &self,
        _ctx: &ProjectCtx,
        _limit: usize,
        _since_days: Option<u32>,
        _max_files_per_commit: usize,
    ) -> std::result::Result<Vec<crate::bridge::CouplingHubEntry>, String> {
        Ok(Vec::new())
    }
    async fn resolve(
        &self,
        _ctx: &ProjectCtx,
        _key: &str,
        _kind_hint: Option<&str>,
    ) -> std::result::Result<crate::bridge::ResolveOutcome, String> {
        // Stub: pretend the key matches itself. Tests that exercise the
        // C2 ambiguity path inject their own bridge.
        Ok(crate::bridge::ResolveOutcome::NotFound)
    }
}

/// NoteEmbeddingProvider stub. `model_version` is informational; `embed_note`
/// is the compute-heavy call — surface it as a stub error so tests that
/// accidentally trigger embedding see it immediately rather than fabricating
/// zeros.
pub struct StubNoteEmbedding;

#[async_trait]
impl NoteEmbeddingProvider for StubNoteEmbedding {
    fn model_version(&self) -> String {
        "stub-embedding-v0".to_string()
    }
    async fn embed_note(&self, _text: &str) -> std::result::Result<EmbeddedNote, String> {
        Err("stub: NoteEmbeddingProvider::embed_note not implemented".into())
    }
}

/// NoteVectorStore stub. `backend` reports `Noop`; capability checks return
/// `Ok(false)`; reads return empties; writes/deletes error loudly.
pub struct StubNoteVectorStore;

#[async_trait]
impl NoteVectorStore for StubNoteVectorStore {
    fn backend(&self) -> NoteVectorBackend {
        NoteVectorBackend::Noop
    }

    async fn can_index(&self, _repo: &NoteRepository) -> DbResult<bool> {
        Ok(false)
    }

    async fn upsert_embedding(
        &self,
        _repo: &NoteRepository,
        _input: UpsertNoteEmbedding<'_>,
    ) -> DbResult<NoteEmbeddingRecord> {
        Err(DbError::Internal(
            "stub: NoteVectorStore::upsert_embedding not implemented".into(),
        ))
    }

    async fn delete_embedding(
        &self,
        _repo: &NoteRepository,
        _note_id: &str,
    ) -> DbResult<()> {
        Err(DbError::Internal(
            "stub: NoteVectorStore::delete_embedding not implemented".into(),
        ))
    }

    async fn query_similar_embeddings(
        &self,
        _repo: &NoteRepository,
        _query_embedding: &[f32],
        _query: EmbeddingQueryContext<'_>,
        _limit: usize,
    ) -> DbResult<Vec<NoteEmbeddingMatch>> {
        Ok(Vec::new())
    }
}

// ── Harness ────────────────────────────────────────────────────────────────────

/// In-memory test harness that owns an `McpState` + a real `DjinnMcpServer`
/// tool router, backed entirely by stubs and an ephemeral Dolt branch.
///
/// Intended for contract-style tests migrating out of `server/src/
/// mcp_contract_tests/`: tests dispatch tools by name with a `serde_json::Value`
/// and assert on the returned JSON, with zero Axum / zero live-actor / zero
/// network contact.
pub struct McpTestHarness {
    state: McpState,
    db: Database,
    server: DjinnMcpServer,
}

impl McpTestHarness {
    /// Build a harness with an in-memory (Dolt-branch) database, `EventBus::noop()`,
    /// a default catalog + health tracker, and strict stubs for every bridge /
    /// provider trait.
    ///
    /// Panics on database bring-up failure — tests should hit a real error
    /// path, not silently swallow one.
    pub async fn new() -> Self {
        let db = Database::open_in_memory().expect("open in-memory test database");
        Self::from_db(db)
    }

    /// Escape hatch for tests that need a pre-seeded DB.
    pub fn from_db(db: Database) -> Self {
        let state = McpState::new(
            db.clone(),
            EventBus::noop(),
            CatalogService::new(),
            HealthTracker::new(),
            Some(Arc::new(StubCoordinator) as Arc<dyn CoordinatorOps>),
            Some(Arc::new(StubSlotPool) as Arc<dyn SlotPoolOps>),
            Some(Arc::new(StubNoteEmbedding) as Arc<dyn NoteEmbeddingProvider>),
            Some(Arc::new(StubNoteVectorStore) as Arc<dyn NoteVectorStore>),
            Arc::new(StubLsp),
            Arc::new(StubRuntime),
            Arc::new(StubGit),
            Arc::new(StubRepoGraph),
        );
        let server = DjinnMcpServer::new(state.clone());
        Self { state, db, server }
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    pub fn state(&self) -> &McpState {
        &self.state
    }

    pub fn server(&self) -> &DjinnMcpServer {
        &self.server
    }

    /// Dispatch an MCP tool by name with a JSON argument object.  Goes
    /// through the same code path production does — `dispatch_tool` is the
    /// name-keyed match against the `#[tool_router]` methods — so what tests
    /// assert on is the live tool surface, not a bespoke test router.
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<Value> {
        self.server
            .dispatch_tool(name, args)
            .await
            .map_err(|e| anyhow!("call_tool({name}) failed: {e}"))
    }
}

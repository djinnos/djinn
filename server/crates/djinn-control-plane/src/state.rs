use std::path::Path;
use std::sync::Arc;

use djinn_core::events::EventBus;
use djinn_core::models::DjinnSettings;
use djinn_db::{
    Database,
    repositories::note::{NoteEmbeddingProvider, NoteVectorStore},
};
use djinn_provider::catalog::{CatalogService, HealthTracker};

use crate::bridge::{
    CoordinatorOps, GitOps, LspOps, RepoGraphOps, RuntimeOps, SemanticQueryEmbedding, SlotPoolOps,
};

/// Subset of application state consumed by the MCP layer.
///
/// Holds the database, catalog, and boxed bridge-trait handles for
/// server-specific actors (coordinator, pool, LSP). The server
/// constructs this from AppState; djinn-control-plane never depends on AppState or
/// any actor type directly.
#[derive(Clone)]
pub struct McpState {
    db: Database,
    event_bus: EventBus,
    catalog: CatalogService,
    health_tracker: HealthTracker,
    coordinator: Option<Arc<dyn CoordinatorOps>>,
    pool: Option<Arc<dyn SlotPoolOps>>,
    embedding_provider: Option<Arc<dyn NoteEmbeddingProvider>>,
    vector_store: Option<Arc<dyn NoteVectorStore>>,
    lsp: Arc<dyn LspOps>,
    runtime: Arc<dyn RuntimeOps>,
    git: Arc<dyn GitOps>,
    repo_graph: Arc<dyn RepoGraphOps>,
}

impl McpState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Database,
        event_bus: EventBus,
        catalog: CatalogService,
        health_tracker: HealthTracker,
        coordinator: Option<Arc<dyn CoordinatorOps>>,
        pool: Option<Arc<dyn SlotPoolOps>>,
        embedding_provider: Option<Arc<dyn NoteEmbeddingProvider>>,
        vector_store: Option<Arc<dyn NoteVectorStore>>,
        lsp: Arc<dyn LspOps>,
        runtime: Arc<dyn RuntimeOps>,
        git: Arc<dyn GitOps>,
        repo_graph: Arc<dyn RepoGraphOps>,
    ) -> Self {
        Self {
            db,
            event_bus,
            catalog,
            health_tracker,
            coordinator,
            pool,
            embedding_provider,
            vector_store,
            lsp,
            runtime,
            git,
            repo_graph,
        }
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    pub fn event_bus(&self) -> EventBus {
        self.event_bus.clone()
    }

    pub fn catalog(&self) -> &CatalogService {
        &self.catalog
    }

    pub fn health_tracker(&self) -> &HealthTracker {
        &self.health_tracker
    }

    pub async fn coordinator(&self) -> Option<Arc<dyn CoordinatorOps>> {
        self.coordinator.clone()
    }

    pub async fn pool(&self) -> Option<Arc<dyn SlotPoolOps>> {
        self.pool.clone()
    }

    pub fn embedding_provider(&self) -> Option<Arc<dyn NoteEmbeddingProvider>> {
        self.embedding_provider.clone()
    }

    pub fn vector_store(&self) -> Option<Arc<dyn NoteVectorStore>> {
        self.vector_store.clone()
    }

    pub fn lsp(&self) -> &Arc<dyn LspOps> {
        &self.lsp
    }

    pub async fn git_actor(
        &self,
        path: &Path,
    ) -> Result<djinn_git::GitActorHandle, djinn_git::GitError> {
        self.git.git_actor(path).await
    }

    pub fn repo_graph(&self) -> &Arc<dyn RepoGraphOps> {
        &self.repo_graph
    }

    pub async fn apply_settings(&self, settings: &DjinnSettings) -> Result<(), String> {
        self.runtime.apply_settings(settings).await
    }

    pub async fn embed_memory_query(
        &self,
        query: &str,
    ) -> Result<Option<SemanticQueryEmbedding>, String> {
        self.runtime.embed_memory_query(query).await
    }

    pub async fn reset_runtime_settings(&self) {
        self.runtime.reset_runtime_settings().await;
    }

    pub async fn persist_model_health_state(&self) {
        self.runtime.persist_model_health_state().await;
    }

    pub async fn apply_environment_config(
        &self,
        project_id: &str,
        config: &djinn_stack::environment::EnvironmentConfig,
    ) -> Result<(), String> {
        self.runtime.apply_environment_config(project_id, config).await
    }
}

// ── Stub impls for test builds ─────────────────────────────────────────────────
// Provide a no-actor McpState for tests that exercise MCP tool handlers
// directly (without a full Axum server).

#[cfg(any(test, feature = "test-support"))]
pub mod stubs {
    #![allow(dead_code, unused_imports)]
    use super::*;
    use crate::bridge::{
        GraphNeighbor, ImpactEntry, LspWarning, PoolStatus, RankedNode, RunningTaskInfo,
    };
    use async_trait::async_trait;
    use djinn_git::{GitActorHandle, GitError};

    pub struct StubCoordinatorOps;
    #[async_trait]
    impl CoordinatorOps for StubCoordinatorOps {
        fn get_status(&self) -> Result<crate::bridge::CoordinatorStatus, String> {
            Err("coordinator not initialized".into())
        }
        async fn trigger_dispatch_for_project(&self, _: &str) -> Result<(), String> {
            Err("coordinator not initialized".into())
        }
    }

    pub struct StubSlotPoolOps;
    #[async_trait]
    impl SlotPoolOps for StubSlotPoolOps {
        async fn get_status(&self) -> Result<PoolStatus, String> {
            Err("slot pool not initialized".into())
        }
        async fn kill_session(&self, _: &str) -> Result<(), String> {
            Err("slot pool not initialized".into())
        }
        async fn session_for_task(&self, _: &str) -> Result<Option<RunningTaskInfo>, String> {
            Err("slot pool not initialized".into())
        }
        async fn has_session(&self, _: &str) -> Result<bool, String> {
            Ok(false)
        }
    }

    pub struct StubLspOps;
    #[async_trait]
    impl LspOps for StubLspOps {
        async fn warnings(&self) -> Vec<LspWarning> {
            vec![]
        }
    }

    pub struct StubRuntimeOps;
    #[async_trait]
    impl RuntimeOps for StubRuntimeOps {
        async fn apply_settings(&self, _: &DjinnSettings) -> Result<(), String> {
            Ok(())
        }
        async fn embed_memory_query(
            &self,
            _: &str,
        ) -> Result<Option<SemanticQueryEmbedding>, String> {
            Ok(None)
        }
        async fn reset_runtime_settings(&self) {}
        async fn persist_model_health_state(&self) {}
        async fn apply_environment_config(
            &self,
            _: &str,
            _: &djinn_stack::environment::EnvironmentConfig,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    pub struct StubGitOps;
    #[async_trait]
    impl GitOps for StubGitOps {
        async fn git_actor(&self, _: &Path) -> Result<GitActorHandle, GitError> {
            Err(GitError::CommandFailed {
                code: 1,
                command: "rev-parse".into(),
                cwd: ".".into(),
                stdout: String::new(),
                stderr: "no repository found".into(),
            })
        }
    }

    pub struct StubRepoGraphOps;
    #[async_trait]
    impl RepoGraphOps for StubRepoGraphOps {
        async fn neighbors(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<crate::bridge::NeighborsResult, String> {
            Ok(crate::bridge::NeighborsResult::Detailed(vec![]))
        }
        async fn ranked(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<RankedNode>, String> {
            Ok(vec![])
        }
        async fn implementations(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: &str,
        ) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
        async fn impact(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: &str,
            _: usize,
            _: Option<&str>,
            _: Option<f64>,
        ) -> Result<crate::bridge::ImpactResult, String> {
            Ok(crate::bridge::ImpactResult::Detailed(vec![]))
        }
        async fn search(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<crate::bridge::SearchHit>, String> {
            Ok(vec![])
        }
        async fn cycles(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<crate::bridge::CycleGroup>, String> {
            Ok(vec![])
        }
        async fn orphans(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<crate::bridge::OrphanEntry>, String> {
            Ok(vec![])
        }
        async fn path(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: &str,
            _: &str,
            _: Option<usize>,
        ) -> Result<Option<crate::bridge::PathResult>, String> {
            Ok(None)
        }
        async fn edges(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<crate::bridge::EdgeEntry>, String> {
            Ok(vec![])
        }
        async fn describe(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: &str,
        ) -> Result<Option<crate::bridge::SymbolDescription>, String> {
            Ok(None)
        }
        async fn context(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: &str,
            _: bool,
        ) -> Result<Option<crate::bridge::SymbolContext>, String> {
            Ok(None)
        }
        async fn status(
            &self,
            _: &crate::bridge::ProjectCtx,
        ) -> Result<crate::bridge::GraphStatus, String> {
            Ok(crate::bridge::GraphStatus {
                project_id: String::new(),
                warmed: false,
                last_warm_at: None,
                pinned_commit: None,
                commits_since_pin: None,
            })
        }
        async fn snapshot(
            &self,
            _: &crate::bridge::ProjectCtx,
            node_cap: usize,
            _: &crate::tools::graph_exclusions::GraphExclusions,
        ) -> Result<crate::bridge::SnapshotPayload, String> {
            Ok(crate::bridge::SnapshotPayload {
                project_id: String::new(),
                git_head: String::new(),
                generated_at: String::new(),
                truncated: false,
                total_nodes: 0,
                total_edges: 0,
                node_cap,
                nodes: vec![],
                edges: vec![],
            })
        }
        async fn symbols_at(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: &str,
            _: u32,
            _: Option<u32>,
        ) -> Result<Vec<crate::bridge::SymbolAtHit>, String> {
            Ok(vec![])
        }
        async fn diff_touches(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: &[crate::bridge::ChangedRange],
        ) -> Result<crate::bridge::DiffTouchesResult, String> {
            Ok(crate::bridge::DiffTouchesResult {
                touched_symbols: vec![],
                affected_files: vec![],
                unknown_files: vec![],
            })
        }
        async fn detect_changes(
            &self,
            _: &crate::bridge::ProjectCtx,
            from_sha: Option<&str>,
            to_sha: Option<&str>,
            _: &[String],
        ) -> Result<crate::bridge::DetectedChangesResult, String> {
            Ok(crate::bridge::DetectedChangesResult {
                from_sha: from_sha.unwrap_or("").to_string(),
                to_sha: to_sha.unwrap_or("").to_string(),
                touched_symbols: vec![],
                by_file: std::collections::BTreeMap::new(),
            })
        }
        async fn api_surface(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<crate::bridge::ApiSurfaceEntry>, String> {
            Ok(vec![])
        }
        async fn boundary_check(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: &[crate::bridge::BoundaryRule],
        ) -> Result<Vec<crate::bridge::BoundaryViolation>, String> {
            Ok(vec![])
        }
        async fn hotspots(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: u32,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<crate::bridge::HotspotEntry>, String> {
            Ok(vec![])
        }
        async fn metrics_at(
            &self,
            _: &crate::bridge::ProjectCtx,
        ) -> Result<crate::bridge::MetricsAtResult, String> {
            Ok(crate::bridge::MetricsAtResult {
                commit: String::new(),
                node_count: 0,
                edge_count: 0,
                cycle_count: 0,
                cycles_by_size_histogram: std::collections::BTreeMap::new(),
                god_object_count: 0,
                orphan_count: 0,
                public_api_count: 0,
                doc_coverage_pct: 0.0,
            })
        }
        async fn dead_symbols(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: &str,
            _: usize,
        ) -> Result<Vec<crate::bridge::DeadSymbolEntry>, String> {
            Ok(vec![])
        }
        async fn deprecated_callers(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: usize,
        ) -> Result<Vec<crate::bridge::DeprecatedHit>, String> {
            Ok(vec![])
        }
        async fn touches_hot_path(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: &[String],
            _: &[String],
            _: &[String],
        ) -> Result<Vec<crate::bridge::HotPathHit>, String> {
            Ok(vec![])
        }
        async fn coupling(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: &str,
            _: usize,
        ) -> Result<Vec<crate::bridge::CouplingEntry>, String> {
            Ok(vec![])
        }
        async fn churn(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: usize,
            _: Option<u32>,
        ) -> Result<Vec<crate::bridge::ChurnEntry>, String> {
            Ok(vec![])
        }
        async fn coupling_hotspots(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: usize,
            _: Option<u32>,
            _: usize,
        ) -> Result<Vec<crate::bridge::CoupledPairEntry>, String> {
            Ok(vec![])
        }
        async fn coupling_hubs(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: usize,
            _: Option<u32>,
            _: usize,
        ) -> Result<Vec<crate::bridge::CouplingHubEntry>, String> {
            Ok(vec![])
        }
        async fn resolve(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: &str,
            _: Option<&str>,
        ) -> Result<crate::bridge::ResolveOutcome, String> {
            Ok(crate::bridge::ResolveOutcome::NotFound)
        }
    }

    /// Build a McpState backed only by an in-memory database (no live actors).
    /// Useful for direct-invocation tests of MCP tool handlers.
    pub fn test_mcp_state(db: Database) -> McpState {
        McpState::new(
            db,
            EventBus::noop(),
            CatalogService::new(),
            HealthTracker::new(),
            None,
            None,
            None,
            None,
            Arc::new(StubLspOps),
            Arc::new(StubRuntimeOps),
            Arc::new(StubGitOps),
            Arc::new(StubRepoGraphOps),
        )
    }

    /// Same as [`test_mcp_state`] but lets the test plug in a concrete
    /// `NoteEmbeddingProvider` / `NoteVectorStore`. Used by the
    /// `memory_repair_embeddings` tests, which need a working embedding path.
    pub fn test_mcp_state_with_embedding(
        db: Database,
        embedding_provider: Option<Arc<dyn NoteEmbeddingProvider>>,
        vector_store: Option<Arc<dyn NoteVectorStore>>,
    ) -> McpState {
        McpState::new(
            db,
            EventBus::noop(),
            CatalogService::new(),
            HealthTracker::new(),
            None,
            None,
            embedding_provider,
            vector_store,
            Arc::new(StubLspOps),
            Arc::new(StubRuntimeOps),
            Arc::new(StubGitOps),
            Arc::new(StubRepoGraphOps),
        )
    }
}

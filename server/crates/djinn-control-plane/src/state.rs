use std::path::Path;
use std::sync::Arc;

use djinn_core::events::EventBus;
use djinn_core::models::DjinnSettings;
use djinn_db::{
    Database,
    repositories::note::{NoteEmbeddingProvider, NoteVectorStore},
};
use djinn_provider::catalog::{builtin, CatalogService, HealthTracker};

use crate::bridge::{
    CoordinatorOps, GitOps, LspOps, MemoryEnrichmentOps, RepoGraphOps, RuntimeOps,
    SemanticQueryEmbedding, SlotPoolOps, TaskrunJobRef,
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
    /// Bridge into `djinn_slot::memory_enrichment` (via the `djinn_agent::actors::slot` facade). `None` when
    /// the server wires a context without the enrichment subsystem (test
    /// harnesses, off-server contexts). The MCP tool degrades to a clear
    /// "not configured" error in that case.
    enrichment_ops: Option<Arc<dyn MemoryEnrichmentOps>>,
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
        Self::with_enrichment(
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
            None,
        )
    }

    /// Full constructor — prefer [`McpState::new`] when the caller doesn't
    /// have an enrichment bridge handy. The server binary uses this
    /// overload to wire the agent-backed implementation; tests that don't
    /// exercise enrichment fall through to the simpler constructor.
    #[allow(clippy::too_many_arguments)]
    pub fn with_enrichment(
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
        enrichment_ops: Option<Arc<dyn MemoryEnrichmentOps>>,
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
            enrichment_ops,
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

    /// Access the memory enrichment bridge. Returns `None` when the server
    /// wired an `McpState` without the enrichment subsystem; the
    /// `memory_run_enrichment` tool surfaces a clean "not configured"
    /// error in that case.
    pub fn enrichment_ops(&self) -> Option<Arc<dyn crate::bridge::MemoryEnrichmentOps>> {
        self.enrichment_ops.clone()
    }

    /// Test-only hook to install an enrichment bridge on an already-built
    /// `McpState`. Production code uses [`McpState::with_enrichment`] at
    /// construction time; tests that stand up state via [`McpState::new`]
    /// (because they don't have a `MemoryEnrichmentOps` handy at the
    /// call site) can use this to wire the bridge in afterwards.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_enrichment_ops(&mut self, ops: Arc<dyn crate::bridge::MemoryEnrichmentOps>) {
        self.enrichment_ops = Some(ops);
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

    /// Recompute fleet slot-pool capacity for the new union of per-user model
    /// selections and trigger a dispatch pass. Delegates to the runtime.
    pub async fn apply_user_model_change(&self) {
        self.runtime.apply_user_model_change().await;
    }

    /// Read the org-wide AI policy (admin-owned singleton). On a read error the
    /// policy degrades to all-defaults (no blocks, flexible) so a transient DB
    /// hiccup never silently hides every provider from members.
    pub async fn org_ai_policy(&self) -> djinn_core::models::OrgAiPolicy {
        let repo = djinn_db::OrgAiPolicyRepository::new(self.db.clone());
        match repo.get().await {
            Ok(policy) => policy,
            Err(e) => {
                tracing::warn!(error = %e, "org_ai_policy: read failed; using defaults");
                djinn_core::models::OrgAiPolicy::default()
            }
        }
    }

    /// The set of subscription provider ids blocked org-wide. Member-facing
    /// provider/model surfaces filter these out; per-user model validation
    /// rejects them. Lowercased for case-insensitive matching.
    pub async fn blocked_subscription_ids(&self) -> std::collections::HashSet<String> {
        self.org_ai_policy()
            .await
            .blocked_subscriptions
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect()
    }

    /// Validate that every model in `models` is present in the catalog exposed
    /// by a provider connected *for `user_id`* (their own credential or the
    /// org-shared fallback) and is not a subscription blocked by org policy.
    /// Used by `user_settings_set` so a user can't select a model on a provider
    /// they haven't connected, an invented model ID, or a subscription an admin
    /// has blocked. Empty selection is always valid.
    pub async fn validate_models_for_user(
        &self,
        models: &[String],
        user_id: Option<&str>,
    ) -> Result<(), String> {
        use std::collections::HashSet;
        if models.is_empty() {
            return Ok(());
        }
        let configured_provider_ids: HashSet<String> = models
            .iter()
            .map(|model| {
                model
                    .split_once('/')
                    .map(|(provider_id, _)| provider_id)
                    .unwrap_or(model.as_str())
                    .to_string()
            })
            .collect();

        // Org-policy gate: reject any model on a blocked subscription before the
        // connectivity check, with a distinct message. We resolve each model to
        // its governable subscription IDENTITY (not just its namespace provider)
        // so a blocked ChatGPT/Codex sub catches `openai/...-codex` models even
        // though they surface under the `openai` namespace — while a plain openai
        // BYO API key stays ungoverned.
        let blocked = self.blocked_subscription_ids().await;
        if !blocked.is_empty() {
            let mut blocked_hits: Vec<String> = models
                .iter()
                .filter_map(|model| {
                    let provider_id = model
                        .split_once('/')
                        .map(|(pid, _)| pid)
                        .unwrap_or(model.as_str());
                    djinn_provider::catalog::builtin::governable_subscription_for_model(
                        provider_id,
                        model,
                    )
                    .filter(|sub| blocked.contains(&sub.to_ascii_lowercase()))
                })
                .collect();
            blocked_hits.sort();
            blocked_hits.dedup();
            if !blocked_hits.is_empty() {
                return Err(format!(
                    "models reference subscriptions blocked by org policy: {}",
                    blocked_hits.join(", ")
                ));
            }
        }

        let repo = djinn_provider::repos::CredentialRepository::new(
            self.db.clone(),
            self.event_bus.clone(),
        );
        let credentials = repo
            .list_for_user(user_id)
            .await
            .map_err(|e| format!("list credentials: {e}"))?;
        let connected = self.catalog.connected_provider_ids(&credentials);

        let mut missing: Vec<String> = configured_provider_ids
            .difference(&connected)
            .cloned()
            .collect();
        missing.sort();
        if !missing.is_empty() {
            return Err(format!(
                "models reference providers you haven't connected: {}",
                missing.join(", ")
            ));
        }

        // `provider_models_connected` folds a merged child (such as
        // `chatgpt_codex`) into its parent namespace. Mirror that exact
        // presentation here so a surfaced `openai/...` child model remains
        // valid while arbitrary IDs under a connected provider do not.
        let catalog_contains = |full_model_id: &str| {
            let Some((provider_id, _)) = full_model_id.split_once('/') else {
                return false;
            };

            std::iter::once(provider_id.to_string())
                .chain(
                    builtin::merged_provider_ids()
                        .into_iter()
                        .filter(|child_id| {
                            builtin::find_builtin_provider(child_id)
                                .and_then(|provider| provider.merge_into)
                                == Some(provider_id)
                        }),
                )
                .any(|source_provider_id| {
                    self.catalog
                        .list_models(&source_provider_id)
                        .into_iter()
                        .any(|model| {
                            let source_prefix = format!("{source_provider_id}/");
                            let source_model_id = model
                                .id
                                .strip_prefix(&source_prefix)
                                .unwrap_or(&model.id);
                            let surfaced_id = format!("{provider_id}/{source_model_id}");
                            surfaced_id == full_model_id
                        })
                })
        };

        let mut absent: Vec<&str> = models
            .iter()
            .map(String::as_str)
            .filter(|model| !catalog_contains(model))
            .collect();
        absent.sort_unstable();
        absent.dedup();
        if absent.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "models are not available in the connected provider catalog: {}",
                absent.join(", ")
            ))
        }
    }

    pub async fn persist_model_health_state(&self) {
        self.runtime.persist_model_health_state().await;
    }

    pub async fn apply_environment_config(
        &self,
        project_id: &str,
        config: &djinn_stack::environment::EnvironmentConfig,
    ) -> Result<(), String> {
        self.runtime
            .apply_environment_config(project_id, config)
            .await
    }

    pub async fn trigger_mirror_refresh(&self, project_id: &str) {
        self.runtime.trigger_mirror_refresh(project_id).await;
    }

    pub async fn enqueue_image_build(&self, image_id: &str) -> Result<(), String> {
        self.runtime.enqueue_image_build(image_id).await
    }

    pub async fn trigger_graph_warm(&self, project_id: &str) {
        self.runtime.trigger_graph_warm(project_id).await;
    }

    /// Best-effort/idempotent foreground deletion of the canonical task-run Job
    /// (`djinn-taskrun-{task_run_id}`), routed through the runtime bridge so
    /// control-plane/agent callers never depend on djinn-k8s directly.
    pub async fn teardown_taskrun_job(&self, task_run_id: &str) -> Result<(), String> {
        self.runtime.teardown_taskrun_job(task_run_id).await
    }

    /// List Djinn task-run Jobs visible to the runtime. The returned structs
    /// are control-plane-owned and contain no Kubernetes API types.
    pub async fn list_taskrun_jobs(&self) -> Result<Vec<TaskrunJobRef>, String> {
        self.runtime.list_taskrun_jobs().await
    }

    /// Best-effort: delete a force-closed task's branch on the local mirror and
    /// the GitHub remote (closing any open PR). Used by the abort cascade.
    pub async fn cleanup_task_branches(&self, task_id: &str) {
        self.runtime.cleanup_task_branches(task_id).await;
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
        async fn start_proposal_refinement(
            &self,
            _: crate::bridge::ProposalRefinementStartRequest,
        ) -> Result<(), String> {
            Err("coordinator not initialized".into())
        }
        async fn demand_proposal_refinement_round(
            &self,
            _: crate::bridge::ProposalRefinementStartRequest,
        ) -> Result<(), String> {
            Err("coordinator not initialized".into())
        }
        async fn resolve_refinement_review(
            &self,
            _: String,
            _: bool,
            _: Option<String>,
        ) -> Result<(), String> {
            Err("coordinator not initialized".into())
        }
        async fn record_supervisor_rework_reopen(
            &self,
            _: &str,
            _: &djinn_core::models::TransitionAction,
            _: Option<&str>,
        ) {
        }
    }

    /// Test-only coordinator stub that accepts refinement starts (returns Ok)
    /// while still rejecting other operations.  Used by `test_mcp_state` so
    /// existing refinement tool tests can run without a real coordinator.
    pub struct StubRefinementAcceptingCoordinator;
    #[async_trait]
    impl CoordinatorOps for StubRefinementAcceptingCoordinator {
        fn get_status(&self) -> Result<crate::bridge::CoordinatorStatus, String> {
            Err("coordinator not initialized".into())
        }
        async fn trigger_dispatch_for_project(&self, _: &str) -> Result<(), String> {
            Err("coordinator not initialized".into())
        }
        async fn start_proposal_refinement(
            &self,
            _: crate::bridge::ProposalRefinementStartRequest,
        ) -> Result<(), String> {
            Ok(())
        }
        async fn demand_proposal_refinement_round(
            &self,
            _: crate::bridge::ProposalRefinementStartRequest,
        ) -> Result<(), String> {
            Ok(())
        }
        async fn resolve_refinement_review(
            &self,
            _: String,
            _: bool,
            _: Option<String>,
        ) -> Result<(), String> {
            Ok(())
        }
        async fn record_supervisor_rework_reopen(
            &self,
            _: &str,
            _: &djinn_core::models::TransitionAction,
            _: Option<&str>,
        ) {
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
        async fn terminate_session(&self, _: &str) -> Result<(), String> {
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
        async fn trigger_mirror_refresh(&self, _: &str) {}
        async fn enqueue_image_build(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn trigger_graph_warm(&self, _: &str) {}
        async fn apply_user_model_change(&self) {}
        async fn teardown_taskrun_job(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn list_taskrun_jobs(&self) -> Result<Vec<TaskrunJobRef>, String> {
            Ok(Vec::new())
        }
        async fn cleanup_task_branches(&self, _: &str) {}
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
        async fn workspaces(
            &self,
            ctx: &crate::bridge::ProjectCtx,
        ) -> Result<crate::bridge::WorkspacesResult, String> {
            Ok(crate::bridge::WorkspacesResult {
                project_id: ctx.id.clone(),
                workspaces: vec![],
            })
        }

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
            _workspace: Option<&str>,
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
            _workspace: Option<&str>,
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
            _workspace: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<crate::bridge::OrphanEntry>, String> {
            Ok(vec![])
        }
        async fn path(
            &self,
            _: &crate::bridge::ProjectCtx,
            _workspace: Option<&str>,
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
                route_parity_enabled: true,
                route_exclusion_config: serde_json::json!({}),
            })
        }
        async fn snapshot(
            &self,
            _: &crate::bridge::ProjectCtx,
            _workspace: Option<&str>,
            _level: crate::bridge::SnapshotLevel,
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
            _workspace: Option<&str>,
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
            _: &str,
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
        async fn complexity(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<crate::bridge::ComplexityResult, String> {
            Ok(crate::bridge::ComplexityResult::Functions(vec![]))
        }
        async fn refactor_candidates(
            &self,
            _: &crate::bridge::ProjectCtx,
            _: Option<u32>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<crate::bridge::RefactorCandidate>, String> {
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
            _workspace: Option<&str>,
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
            Some(Arc::new(StubRefinementAcceptingCoordinator)),
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

    /// Same as [`test_mcp_state`] but lets the test plug in a concrete
    /// `EventBus`. Used by the dispatch-pause read-only regression tests
    /// (in `server_tests.rs`), which need to observe whether a status call
    /// emits a `dispatch_pause.changed` envelope. The default `noop()` bus
    /// drops every envelope, so it cannot prove the absence of an event
    /// emission — tests that need that guarantee must build a recording
    /// bus and route the server's `McpState` through it.
    pub fn test_mcp_state_with_event_bus(db: Database, event_bus: EventBus) -> McpState {
        McpState::new(
            db,
            event_bus,
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
}

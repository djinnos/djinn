//! Compatibility shim: `AgentContext` and related types.
//!
//! These types mirror the definitions in `djinn-agent::context` so that
//! coordinator code can construct a maintenance-context inline without
//! depending on `djinn-agent`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use djinn_control_plane::bridge;
use djinn_core::events::EventBus;
use djinn_db::Database;
use djinn_git::GitActorHandle;
use djinn_orchestration_types::coordinator::BackgroundWorkTracker;
use djinn_provider::catalog::{CatalogService, HealthTracker};
use djinn_runtime::GraphWarmerService;
use djinn_workspace::MirrorManager;
use tokio::sync::Mutex;

use crate::handle::CoordinatorHandle;

/// Shared tracker for per-task last-activity timestamps (unix seconds).
pub type ActivityTracker = Arc<Mutex<HashMap<String, Arc<AtomicU64>>>>;

/// Configuration for the periodic reconciliation sweep.
#[derive(Debug, Clone)]
pub struct ReconciliationSweepConfig {
    pub enabled: bool,
    pub dry_run: bool,
    pub grace_period: std::time::Duration,
}

impl Default for ReconciliationSweepConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dry_run: true,
            grace_period: std::time::Duration::from_secs(600),
        }
    }
}

impl ReconciliationSweepConfig {
    pub fn from_env() -> Self {
        let enabled = std::env::var("DJINN_RECONCILIATION_SWEEP_ENABLED")
            .map(|v| parse_bool_env(&v))
            .unwrap_or(false);
        let dry_run = std::env::var("DJINN_RECONCILIATION_SWEEP_DRY_RUN")
            .map(|v| parse_bool_env(&v))
            .unwrap_or(true);
        let grace_period_secs = std::env::var("DJINN_RECONCILIATION_SWEEP_GRACE_PERIOD_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(600);

        Self {
            enabled,
            dry_run,
            grace_period: std::time::Duration::from_secs(grace_period_secs),
        }
    }
}

fn parse_bool_env(val: &str) -> bool {
    matches!(
        val.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}

/// Subset of application state required by coordinator code.
///
/// Mirrors `djinn-agent::context::AgentContext`.  Coordinator code constructs
/// this inline for maintenance / merge operations.
#[derive(Clone)]
pub struct AgentContext {
    pub db: Database,
    pub event_bus: EventBus,
    pub git_actors: Arc<Mutex<HashMap<PathBuf, GitActorHandle>>>,
    pub background_work_tasks: BackgroundWorkTracker,
    pub role_registry: Arc<crate::roles_compat::RoleRegistry>,
    pub health_tracker: HealthTracker,
    pub file_time: Arc<crate::file_time::FileTime>,
    pub lsp: djinn_lsp::LspManager,
    pub catalog: CatalogService,
    pub coordinator: Arc<Mutex<Option<CoordinatorHandle>>>,
    pub active_tasks: ActivityTracker,
    pub task_ops_project_path_override: Option<PathBuf>,
    pub working_root: Option<PathBuf>,
    pub graph_warmer: Option<Arc<dyn GraphWarmerService>>,
    pub repo_graph_ops: Option<Arc<dyn bridge::RepoGraphOps>>,
    pub runtime_ops: Option<Arc<dyn bridge::RuntimeOps>>,
    pub cargo_target_runs_root: Option<PathBuf>,
    pub mirror: Option<Arc<MirrorManager>>,
    pub rpc_registry: Option<Arc<djinn_supervisor::ConnectionRegistry>>,
    pub default_project_id: Option<String>,
    pub reconciliation_sweep: ReconciliationSweepConfig,
}

impl AgentContext {
    pub fn working_root_for(&self, fallback: &Path) -> PathBuf {
        match self.working_root.as_deref() {
            Some(p) => p.to_path_buf(),
            None => fallback.to_path_buf(),
        }
    }

    /// Resolve a GitActorHandle for the given project directory.
    pub async fn git_actor(
        &self,
        project_dir: &Path,
    ) -> Result<GitActorHandle, djinn_git::GitError> {
        let mut actors = self.git_actors.lock().await;
        djinn_git::get_or_spawn(&mut actors, project_dir)
    }
}

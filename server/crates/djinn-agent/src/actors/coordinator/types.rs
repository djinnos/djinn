use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use super::consolidation::ConsolidationRunner;
use crate::actors::slot::SlotPoolHandle;
use crate::roles::RoleRegistry;
use djinn_core::events::DjinnEventEnvelope;
use djinn_provider::catalog::CatalogService;
use djinn_provider::catalog::health::HealthTracker;
use djinn_db::Database;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Shared tracker for in-flight verification pipelines.  The verification
/// spawner registers task IDs here; the coordinator checks it during stuck
/// detection so it can distinguish live pipelines from orphans after restart.
pub type VerificationTracker = Arc<std::sync::Mutex<HashSet<String>>>;

pub struct CoordinatorDeps {
    pub events_tx: broadcast::Sender<DjinnEventEnvelope>,
    pub cancel: CancellationToken,
    pub db: Database,
    pub pool: SlotPoolHandle,
    pub catalog: CatalogService,
    pub health: HealthTracker,
    pub role_registry: Arc<RoleRegistry>,
    pub verification_tracker: VerificationTracker,
    pub lsp: crate::lsp::LspManager,
    pub(super) consolidation_runner: Option<Arc<dyn ConsolidationRunner>>,
}

impl CoordinatorDeps {
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        events_tx: broadcast::Sender<DjinnEventEnvelope>,
        cancel: CancellationToken,
        db: Database,
        pool: SlotPoolHandle,
        catalog: CatalogService,
        health: HealthTracker,
        role_registry: Arc<RoleRegistry>,
        verification_tracker: VerificationTracker,
        lsp: crate::lsp::LspManager,
    ) -> Self {
        Self {
            events_tx,
            cancel,
            db,
            pool,
            catalog,
            health,
            role_registry,
            verification_tracker,
            lsp,
            consolidation_runner: None,
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn with_consolidation_runner(mut self, runner: Arc<dyn ConsolidationRunner>) -> Self {
        self.consolidation_runner = Some(runner);
        self
    }
}

// ─── Constants ───────────────────────────────────────────────────────────────

/// Interval between stuck-detection passes (AGENT-08).
pub(super) const STUCK_INTERVAL: Duration = Duration::from_secs(30);
pub(super) const STALE_SWEEP_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Minimum cooldown between idle-time memory consolidation sweeps (ADR-048 §3A).
pub(super) const IDLE_CONSOLIDATION_COOLDOWN: Duration = Duration::from_secs(300);

pub(super) const TASK_OUTCOME_CONFIDENCE_ACTIVITY: &str = "task_outcome_confidence";
pub(super) const TASK_OUTCOME_CONFIDENCE_SIGNAL: f64 = 0.1;
pub(super) const TASK_OUTCOME_REOPEN_COUNT: &str = "reopen_count";
pub(super) const TASK_OUTCOME_FAILED_CLOSE: &str = "failed_closed";

/// Cooldown before re-dispatching a task that failed lifecycle setup
/// (e.g. missing credential).  Prevents hot dispatch loops.
pub(super) const DISPATCH_COOLDOWN: Duration = Duration::from_secs(60);

/// If a task becomes dispatch-ready again within this threshold of its last
/// dispatch, it is considered a rapid failure and placed in cooldown.
pub(super) const RAPID_FAILURE_THRESHOLD: Duration = Duration::from_secs(10);
#[cfg(test)]
pub(super) const DEFAULT_MODEL_ID: &str = "test/mock";

// ─── Error ───────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("actor channel closed")]
    ActorDead,
    #[error("no response from actor")]
    NoResponse,
}

// ─── Public types ────────────────────────────────────────────────────────────

/// Snapshot of coordinator runtime state (returned by `CoordinatorHandle::get_status`).
#[derive(Debug, Clone)]
pub struct CoordinatorStatus {
    pub paused: bool,
    pub tasks_dispatched: u64,
    pub sessions_recovered: u64,
    /// Per-project health errors (project_id → error message).
    /// Only populated when queried for a specific project.
    pub unhealthy_projects: HashMap<String, String>,
    /// Tasks merged per hour per epic (rolling 1-hour window).
    pub epic_throughput: HashMap<String, usize>,
    /// Per-project PR creation errors (project_id → error message).
    /// Populated when GitHub PR creation fails (e.g. org OAuth restrictions).
    pub pr_errors: HashMap<String, String>,
}

/// Internal snapshot published via `watch` channel so `get_status()` reads
/// never queue behind long-running dispatch passes.
#[derive(Debug, Clone)]
pub(super) struct SharedCoordinatorState {
    pub(super) paused_projects: HashSet<String>,
    pub(super) unhealthy_project_ids: HashSet<String>,
    pub(super) unhealthy_project_errors: HashMap<String, String>,
    pub(super) dispatched: u64,
    pub(super) recovered: u64,
    /// Tasks merged per hour per epic (rolling window snapshot).
    pub(super) epic_throughput: HashMap<String, usize>,
    /// Per-project PR creation errors (project_id → error message).
    pub(super) pr_errors: HashMap<String, String>,
}

impl SharedCoordinatorState {
    pub(super) fn to_status(&self, project_id: Option<&str>) -> CoordinatorStatus {
        let paused = match project_id {
            Some(id) => {
                self.unhealthy_project_ids.contains(id) || self.paused_projects.contains(id)
            }
            None => !self.paused_projects.is_empty(),
        };
        let unhealthy_projects = match project_id {
            Some(id) => self
                .unhealthy_project_errors
                .get(id)
                .map(|err| {
                    let mut m = HashMap::new();
                    m.insert(id.to_string(), err.clone());
                    m
                })
                .unwrap_or_default(),
            None => self.unhealthy_project_errors.clone(),
        };
        let pr_errors = match project_id {
            Some(id) => self
                .pr_errors
                .get(id)
                .map(|err| {
                    let mut m = HashMap::new();
                    m.insert(id.to_string(), err.clone());
                    m
                })
                .unwrap_or_default(),
            None => self.pr_errors.clone(),
        };
        CoordinatorStatus {
            paused,
            tasks_dispatched: self.dispatched,
            sessions_recovered: self.recovered,
            unhealthy_projects,
            epic_throughput: self.epic_throughput.clone(),
            pr_errors,
        }
    }
}

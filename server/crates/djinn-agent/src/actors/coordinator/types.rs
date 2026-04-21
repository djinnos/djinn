use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

use super::consolidation::ConsolidationRunner;
use crate::actors::slot::SlotPoolHandle;
use crate::roles::RoleRegistry;
use djinn_core::events::DjinnEventEnvelope;
use djinn_db::Database;
use djinn_provider::catalog::CatalogService;
use djinn_provider::catalog::health::HealthTracker;
use djinn_runtime::GraphWarmerService;
use djinn_workspace::MirrorManager;
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
    /// Optional ADR-051 §3 canonical-graph warmer.  When `Some`, the
    /// coordinator tick loop calls `trigger` for every dispatch-enabled
    /// project on a 10-minute cadence (see `GRAPH_REFRESH_INTERVAL`).  Tests
    /// and off-server contexts leave this `None`, which makes the proactive
    /// refresh tick branch a no-op.
    pub graph_warmer: Option<Arc<dyn GraphWarmerService>>,
    pub(super) consolidation_runner: Option<Arc<dyn ConsolidationRunner>>,
    /// Shared bare-mirror manager. Threaded into the synthesized `AgentContext`
    /// built inside `process_approved_tasks` so the direct-push merge fallback
    /// can clone the ephemeral workspace from the mirror. `None` in test
    /// contexts — the direct-push path bails cleanly in that case.
    pub mirror: Option<Arc<MirrorManager>>,
}

impl CoordinatorDeps {
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
            graph_warmer: None,
            consolidation_runner: None,
            mirror: None,
        }
    }

    /// Inject the production canonical-graph warmer, enabling the ADR-051 §3
    /// proactive staleness refresh tick in the coordinator loop.  Tests and
    /// off-server contexts that omit this leave the tick as a no-op.
    pub fn with_graph_warmer(mut self, warmer: Arc<dyn GraphWarmerService>) -> Self {
        self.graph_warmer = Some(warmer);
        self
    }

    /// Inject the production `MirrorManager`, enabling the mirror-native
    /// direct-push merge fallback. Off-server tests skip this and the fallback
    /// returns a descriptive error instead of crashing.
    pub fn with_mirror(mut self, mirror: Arc<MirrorManager>) -> Self {
        self.mirror = Some(mirror);
        self
    }
}

// ─── Constants ───────────────────────────────────────────────────────────────

/// Interval between stuck-detection passes (AGENT-08).
pub(super) const STUCK_INTERVAL: Duration = Duration::from_secs(30);
pub(super) const STALE_SWEEP_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// ADR-051 §7 — stale auto-dispatch safety net.  Epics that fell through
/// all event-driven auto-dispatch paths are rechecked at this interval.
pub(super) const AUTO_DISPATCH_SWEEP_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// ADR-051 §3 proactive canonical-graph refresh cadence.
///
/// Every 10 minutes the coordinator asks the canonical-graph warmer to
/// refresh any project whose cache has fallen behind `origin/main`.  The
/// warmer is a no-op on cold caches (those are handled by the
/// first-consumer-demand path in `mcp_bridge::maybe_kick_background_warm`)
/// and on warm caches with `commits_since_pin == 0`, so this tick is cheap
/// for projects that are already current.
pub(super) const GRAPH_REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);

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
    pub tasks_dispatched: u64,
    pub sessions_recovered: u64,
    /// Tasks merged per hour per epic (rolling 1-hour window).
    pub epic_throughput: HashMap<String, usize>,
    /// Per-project PR creation errors (project_id → error message).
    /// Populated when GitHub PR creation fails (e.g. org OAuth restrictions).
    pub pr_errors: HashMap<String, String>,
    /// Shared suppression window propagated from provider rate-limit retries.
    pub rate_limited_until: Option<StdInstant>,
}

/// Internal snapshot published via `watch` channel so `get_status()` reads
/// never queue behind long-running dispatch passes.
#[derive(Debug, Clone)]
pub(super) struct SharedCoordinatorState {
    pub(super) dispatched: u64,
    pub(super) recovered: u64,
    /// Tasks merged per hour per epic (rolling window snapshot).
    pub(super) epic_throughput: HashMap<String, usize>,
    /// Per-project PR creation errors (project_id → error message).
    pub(super) pr_errors: HashMap<String, String>,
    pub(super) rate_limited_until: Option<StdInstant>,
}

impl SharedCoordinatorState {
    pub(super) fn to_status(&self) -> CoordinatorStatus {
        CoordinatorStatus {
            tasks_dispatched: self.dispatched,
            sessions_recovered: self.recovered,
            epic_throughput: self.epic_throughput.clone(),
            pr_errors: self.pr_errors.clone(),
            rate_limited_until: self.rate_limited_until,
        }
    }
}

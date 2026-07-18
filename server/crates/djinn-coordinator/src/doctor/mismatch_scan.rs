//! Leader-owned, coalescing runner for the persisted board-health mismatch pass.
//!
//! The mutex protects only idle/running/pending bookkeeping. Database work runs
//! outside it, so an arbitrary burst of triggers becomes one pending rerun.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use djinn_core::clock::{Clock, SystemClock};

static NEXT_LEADER_EPOCH: AtomicI64 = AtomicI64::new(1);

use tokio::sync::Mutex;

use djinn_db::{BoardHealthMismatchCandidate, Database, TaskRepository};

pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, Debug)]
pub enum Trigger {
    Timer,
    Api,
}

impl Trigger {
    fn label(self) -> &'static str {
        match self {
            Self::Timer => "timer",
            Self::Api => "api",
        }
    }
}

#[derive(Default)]
struct FlightState {
    running: bool,
    pending: bool,
    last_timer: Option<std::time::Instant>,
}

/// Shared by the coordinator's leader tick and the control-plane handle.
#[derive(Clone)]
pub struct MismatchScanCoordinator {
    db: Database,
    events: djinn_core::events::EventBus,
    state: Arc<Mutex<FlightState>>,
    leader_epoch: i64,
}

impl MismatchScanCoordinator {
    pub fn new(db: Database, events: djinn_core::events::EventBus) -> Self {
        // Monotonic process epoch fences stale page commits without labels.
        let leader_epoch = NEXT_LEADER_EPOCH.fetch_add(1, Ordering::Relaxed);
        Self {
            db,
            events,
            state: Arc::new(Mutex::new(FlightState::default())),
            leader_epoch,
        }
    }

    /// Start a pass if idle, otherwise retain exactly one follow-up request.
    pub async fn trigger(&self, trigger: Trigger) {
        let mut state = self.state.lock().await;
        if matches!(trigger, Trigger::Timer) {
            let now = SystemClock::new().now_instant();
            if state
                .last_timer
                .is_some_and(|last| now.duration_since(last) < DEFAULT_INTERVAL)
            {
                return;
            }
            state.last_timer = Some(now);
        }
        if state.running {
            state.pending = true;
            djinn_telemetry::board_health_mismatch::record_coalesced(trigger.label());
            return;
        }
        state.running = true;
        drop(state);

        let runner = self.clone();
        tokio::spawn(async move { runner.run_until_quiet(trigger).await });
    }

    async fn run_until_quiet(self, trigger: Trigger) {
        loop {
            self.run_once(trigger).await;
            let mut state = self.state.lock().await;
            if state.pending {
                state.pending = false;
                drop(state);
                continue;
            }
            state.running = false;
            break;
        }
    }

    async fn run_once(&self, trigger: Trigger) {
        let started = SystemClock::new().now_instant();
        let repo = TaskRepository::new(self.db.clone(), self.events.clone());
        let state = match repo
            .start_or_resume_board_health_mismatch_pass(self.leader_epoch)
            .await
        {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(error = %error, trigger = trigger.label(), "board-health mismatch scan could not start or resume");
                djinn_telemetry::board_health_mismatch::record_outcome("error", trigger.label());
                return;
            }
        };
        djinn_telemetry::board_health_mismatch::record_pass_age(state.pass_started_at.as_deref());

        loop {
            let page = match repo
                .load_board_health_mismatch_page(self.leader_epoch)
                .await
            {
                Ok(page) => page,
                Err(error) => {
                    tracing::warn!(error = %error, "board-health mismatch scan page load failed; pass remains resumable");
                    djinn_telemetry::board_health_mismatch::record_outcome(
                        "error",
                        trigger.label(),
                    );
                    return;
                }
            };
            if page.candidates.is_empty() {
                match repo
                    .complete_board_health_mismatch_pass(self.leader_epoch)
                    .await
                {
                    Ok(completed) => {
                        djinn_telemetry::board_health_mismatch::record_duration(started.elapsed());
                        djinn_telemetry::board_health_mismatch::record_pass_age(
                            completed.pass_started_at.as_deref(),
                        );
                        djinn_telemetry::board_health_mismatch::record_outcome(
                            "complete",
                            trigger.label(),
                        );
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "board-health mismatch scan completion failed; pass remains resumable");
                        djinn_telemetry::board_health_mismatch::record_outcome(
                            "error",
                            trigger.label(),
                        );
                    }
                }
                return;
            }

            let expected_cursor = page.state.cursor_id.clone();
            let page_last_id = page.candidates.last().map(|candidate| candidate.id.clone());
            let rows = page.candidates.len();
            // Evaluation is deliberately pure. Do it before advancing the durable
            // cursor so cancellation/error retries the same page at least once.
            let _mismatches = page
                .candidates
                .iter()
                .filter(|candidate| is_role_tool_mismatch(candidate))
                .count();
            drop(page);

            let Some(page_last_id) = page_last_id else {
                return;
            };
            if let Err(error) = repo
                .commit_board_health_mismatch_page(
                    self.leader_epoch,
                    expected_cursor.as_deref(),
                    &page_last_id,
                )
                .await
            {
                tracing::warn!(error = %error, "board-health mismatch scan page evaluation was not committed; pass remains resumable");
                djinn_telemetry::board_health_mismatch::record_outcome("error", trigger.label());
                return;
            }
            djinn_telemetry::board_health_mismatch::record_page(rows);
            tokio::task::yield_now().await;
        }
    }
}

fn is_role_tool_mismatch(candidate: &BoardHealthMismatchCandidate) -> bool {
    djinn_db::evaluate_board_health_mismatch_candidate(candidate)
}

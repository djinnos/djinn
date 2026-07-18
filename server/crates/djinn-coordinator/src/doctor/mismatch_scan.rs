//! Leader-owned, coalescing runner for the persisted board-health mismatch pass.
//!
//! The mutex protects only idle/running/pending bookkeeping. Database work runs
//! outside it, so an arbitrary burst of triggers becomes one pending rerun.

use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
use std::future::Future;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use djinn_core::clock::{Clock, SystemClock};

use tokio::sync::Mutex;

#[cfg(test)]
use tokio::sync::Semaphore;

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

/// Test-only page-source hook for deterministic active-query instrumentation.
#[cfg(test)]
struct PageQueryProbe {
    active: AtomicUsize,
    max_active: AtomicUsize,
    queries: AtomicUsize,
    block_first: AtomicBool,
    // Semaphores retain permits, unlike Notify::notify_waiters. This makes the
    // test handshakes safe regardless of whether a waiter starts before or
    // after the page query reaches its hold point.
    entered: Semaphore,
    release: Semaphore,
}

#[cfg(test)]
impl PageQueryProbe {
    fn blocking_first_query() -> Self {
        Self {
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            queries: AtomicUsize::new(0),
            block_first: AtomicBool::new(true),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }

    async fn load<T>(&self, page_query: impl Future<Output = T>) -> T {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        self.queries.fetch_add(1, Ordering::SeqCst);
        self.entered.add_permits(1);
        if self.block_first.swap(false, Ordering::SeqCst) {
            self.release
                .acquire()
                .await
                .expect("page-query release semaphore is never closed")
                .forget();
        }
        let result = page_query.await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        result
    }

    async fn wait_until_first_query_is_held(&self) {
        self.entered
            .acquire()
            .await
            .expect("page-query entered semaphore is never closed")
            .forget();
        assert_eq!(self.active.load(Ordering::SeqCst), 1);
    }
}

/// Shared by the coordinator's leader tick and the control-plane handle.
#[derive(Clone)]
pub struct MismatchScanCoordinator {
    db: Database,
    events: djinn_core::events::EventBus,
    state: Arc<Mutex<FlightState>>,
    /// Allocated from a database sequence on first use, so it is monotonic
    /// across process restarts and distinct leader pods.
    leader_epoch: Arc<Mutex<Option<i64>>>,
    #[cfg(test)]
    page_query_probe: Option<Arc<PageQueryProbe>>,
}

impl MismatchScanCoordinator {
    pub fn new(db: Database, events: djinn_core::events::EventBus) -> Self {
        Self {
            db,
            events,
            state: Arc::new(Mutex::new(FlightState::default())),
            leader_epoch: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            page_query_probe: None,
        }
    }

    #[cfg(test)]
    fn new_with_page_query_probe(
        db: Database,
        events: djinn_core::events::EventBus,
        page_query_probe: Arc<PageQueryProbe>,
    ) -> Self {
        let mut coordinator = Self::new(db, events);
        coordinator.page_query_probe = Some(page_query_probe);
        coordinator
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
        let repo = TaskRepository::new(self.db.clone(), self.events.clone());
        let leader_epoch = {
            let mut epoch = self.leader_epoch.lock().await;
            match *epoch {
                Some(epoch) => epoch,
                None => match repo.next_board_health_mismatch_leader_epoch().await {
                    Ok(next) => {
                        *epoch = Some(next);
                        next
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, trigger = trigger.label(), "board-health mismatch scan could not allocate a leader epoch");
                        djinn_telemetry::board_health_mismatch::record_outcome(
                            "error",
                            trigger.label(),
                        );
                        return;
                    }
                },
            }
        };
        let state = match repo
            .start_or_resume_board_health_mismatch_pass(leader_epoch)
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
            #[cfg(test)]
            let page_result = if let Some(probe) = &self.page_query_probe {
                probe
                    .load(repo.load_board_health_mismatch_page(leader_epoch))
                    .await
            } else {
                repo.load_board_health_mismatch_page(leader_epoch).await
            };
            #[cfg(not(test))]
            let page_result = repo.load_board_health_mismatch_page(leader_epoch).await;
            let page = match page_result {
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
                match repo.complete_board_health_mismatch_pass(leader_epoch).await {
                    Ok(completed) => {
                        // Completion derives this from the persisted pass start.
                        // A local timer would under-report a restarted pass.
                        let duration_ms =
                            completed.last_pass_duration_ms.unwrap_or(0).max(0) as u64;
                        djinn_telemetry::board_health_mismatch::record_duration(
                            Duration::from_millis(duration_ms),
                        );
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
                    leader_epoch,
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

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;
    use tokio::sync::Barrier;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mixed_trigger_storm_coalesces_behind_one_active_page_query() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let project_id = uuid::Uuid::now_v7().to_string();
        djinn_db::test_support::seed_project(
            &db,
            &project_id,
            &format!("mismatch-storm-{project_id}"),
        )
        .await;
        djinn_db::test_support::seed_board_health_mismatch_candidate(
            &db,
            &project_id,
            "00000000-0000-0000-0000-000000000001",
        )
        .await;
        let probe = Arc::new(PageQueryProbe::blocking_first_query());
        let coordinator =
            MismatchScanCoordinator::new_with_page_query_probe(db, EventBus::noop(), probe.clone());
        coordinator.trigger(Trigger::Api).await;
        probe.wait_until_first_query_is_held().await;

        let barrier = Arc::new(Barrier::new(33));
        let mut joins = Vec::new();
        for i in 0..32 {
            let coordinator = coordinator.clone();
            let barrier = barrier.clone();
            joins.push(tokio::spawn(async move {
                barrier.wait().await;
                coordinator
                    .trigger(if i % 2 == 0 {
                        Trigger::Api
                    } else {
                        Trigger::Timer
                    })
                    .await;
            }));
        }
        barrier.wait().await;
        for join in joins {
            join.await.unwrap();
        }
        // The first page is still held, and the latch can represent only one
        // follow-up run regardless of how many timer/API triggers arrived.
        assert_eq!(probe.active.load(Ordering::SeqCst), 1);
        assert!(coordinator.state.lock().await.pending);
        probe.release.add_permits(1);
        while coordinator.state.lock().await.running {
            tokio::task::yield_now().await;
        }
        let state = coordinator.state.lock().await;
        assert!(!state.running);
        assert!(!state.pending);
        // Each one-row pass has a data page and an empty completion page. The
        // storm coalesces to exactly one additional pass rather than 32 runs.
        assert_eq!(probe.queries.load(Ordering::SeqCst), 4);
        assert_eq!(probe.max_active.load(Ordering::SeqCst), 1);
    }
}

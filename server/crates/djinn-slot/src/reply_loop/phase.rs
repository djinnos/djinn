//! Session-local, non-overlapping provider and tool phase telemetry.
//!
//! Identity remains a tracing concern; Prometheus receives only the bounded phase
//! and role labels accepted by `djinn_telemetry::agent_session_phase`.

use std::sync::Arc;
use std::time::Instant;

use djinn_core::clock::Clock;
use djinn_telemetry::agent_session_phase;

use crate::host::SlotContext;

/// The only session roles admitted to phase telemetry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPhaseRole {
    Worker,
    Reviewer,
    Planner,
    Refinement,
}

impl SessionPhaseRole {
    /// Maps a runtime role name into the collector's closed label domain.
    pub fn from_role_name(role_name: &str) -> Option<Self> {
        match role_name {
            agent_session_phase::ROLE_WORKER => Some(Self::Worker),
            agent_session_phase::ROLE_REVIEWER => Some(Self::Reviewer),
            agent_session_phase::ROLE_PLANNER => Some(Self::Planner),
            agent_session_phase::ROLE_REFINEMENT => Some(Self::Refinement),
            _ => None,
        }
    }

    const fn metric_label(self) -> &'static str {
        match self {
            Self::Worker => agent_session_phase::ROLE_WORKER,
            Self::Reviewer => agent_session_phase::ROLE_REVIEWER,
            Self::Planner => agent_session_phase::ROLE_PLANNER,
            Self::Refinement => agent_session_phase::ROLE_REFINEMENT,
        }
    }
}

/// A bounded, mutually exclusive session phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPhase {
    ProviderWait,
    ToolExecution,
}

impl SessionPhase {
    const fn metric_label(self) -> &'static str {
        match self {
            Self::ProviderWait => agent_session_phase::PHASE_PROVIDER_WAIT,
            Self::ToolExecution => agent_session_phase::PHASE_TOOL_EXECUTION,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ActivePhase {
    phase: SessionPhase,
    started_at: Instant,
}

/// Tracks one session's provider and tool intervals using `SlotContext::clock`.
///
/// Unknown roles are suppressed rather than becoming unbounded metric labels.
pub struct SessionPhaseTracker {
    clock: Arc<dyn Clock>,
    role: Option<SessionPhaseRole>,
    active: Option<ActivePhase>,
    tool_depth: usize,
    finished: bool,
    #[cfg(test)]
    emitted: std::sync::Arc<
        std::sync::Mutex<Vec<(SessionPhase, SessionPhaseRole, std::time::Duration)>>,
    >,
}

impl SessionPhaseTracker {
    /// Creates a tracker backed by [`SlotContext::clock`].
    pub fn new(ctx: &SlotContext, role_name: &str) -> Self {
        Self {
            clock: Arc::clone(&ctx.clock),
            role: SessionPhaseRole::from_role_name(role_name),
            active: None,
            tool_depth: 0,
            finished: false,
            #[cfg(test)]
            emitted: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Starts provider wait, closing the previous interval at the same instant.
    pub fn enter_provider_wait(&mut self) {
        self.enter(SessionPhase::ProviderWait);
    }

    /// Starts tool execution. Nested entries only increase depth and never add
    /// intervals or overlap the outer tool interval.
    pub fn enter_tool_execution(&mut self) {
        if self.finished || self.role.is_none() {
            return;
        }
        if self.tool_depth == 0 {
            self.enter(SessionPhase::ToolExecution);
        }
        self.tool_depth = self.tool_depth.saturating_add(1);
    }

    /// Completes one level of tool dispatch.
    ///
    /// Returning from the outermost dispatch closes its interval immediately,
    /// so prompt assembly and other local work before a later provider entry
    /// are not attributed to tool execution.
    pub fn exit_tool_execution(&mut self) {
        if self.tool_depth == 0 {
            return;
        }

        self.tool_depth -= 1;
        if self.tool_depth == 0
            && self
                .active
                .is_some_and(|active| active.phase == SessionPhase::ToolExecution)
        {
            self.close_active(self.clock.now_instant());
        }
    }

    /// Atomically completes the outermost tool dispatch and starts provider wait.
    ///
    /// Use this at a direct tool-to-provider handoff. A single monotonic reading
    /// closes the tool interval and starts provider wait, so neither a gap nor an
    /// overlap can be introduced by two independent clock reads. For a return to
    /// local prompt/DB/orchestration work, use [`Self::exit_tool_execution`]
    /// instead; it closes tool execution without starting another phase.
    pub fn exit_tool_execution_to_provider_wait(&mut self) {
        if self.finished || self.role.is_none() || self.tool_depth == 0 {
            return;
        }

        self.tool_depth -= 1;
        if self.tool_depth != 0 {
            return;
        }

        let now = self.clock.now_instant();
        if self
            .active
            .is_some_and(|active| active.phase == SessionPhase::ToolExecution)
        {
            self.close_active(now);
        }
        self.active = Some(ActivePhase {
            phase: SessionPhase::ProviderWait,
            started_at: now,
        });
    }

    /// Flushes the active interval exactly once.
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.close_active(self.clock.now_instant());
    }

    fn enter(&mut self, next: SessionPhase) {
        if self.finished || self.role.is_none() {
            return;
        }
        let now = self.clock.now_instant();
        if self.active.is_some_and(|active| active.phase == next) {
            return;
        }
        self.close_active(now);
        self.active = Some(ActivePhase {
            phase: next,
            started_at: now,
        });
    }

    fn close_active(&mut self, ended_at: Instant) {
        let Some(active) = self.active.take() else {
            return;
        };
        let Some(role) = self.role else {
            return;
        };
        let duration = ended_at.saturating_duration_since(active.started_at);
        agent_session_phase::add_phase_duration(
            active.phase.metric_label(),
            role.metric_label(),
            duration,
        );
        #[cfg(test)]
        if let Ok(mut emitted) = self.emitted.lock() {
            emitted.push((active.phase, role, duration));
        }
    }
}

impl Drop for SessionPhaseTracker {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime};

    use djinn_core::clock::{Clock, TestClock};

    use super::{SessionPhase, SessionPhaseRole, SessionPhaseTracker};

    type Intervals = Arc<Mutex<Vec<(SessionPhase, SessionPhaseRole, Duration)>>>;

    fn tracker(role: &str) -> (Arc<TestClock>, SessionPhaseTracker, Intervals) {
        let clock = Arc::new(TestClock::new(SystemTime::UNIX_EPOCH, Instant::now()));
        let intervals = Arc::new(Mutex::new(Vec::new()));
        let tracker = SessionPhaseTracker {
            clock: clock.clone(),
            role: SessionPhaseRole::from_role_name(role),
            active: None,
            tool_depth: 0,
            finished: false,
            emitted: intervals.clone(),
        };
        (clock, tracker, intervals)
    }

    fn emitted(intervals: &Intervals) -> Vec<(SessionPhase, SessionPhaseRole, Duration)> {
        intervals.lock().unwrap().clone()
    }

    /// A fake clock whose monotonic value advances on every read. This detects
    /// accidental independent reads for a transition that must be atomic.
    struct SequentialClock {
        now: Mutex<Instant>,
        instant_reads: AtomicUsize,
    }

    impl SequentialClock {
        fn new(now: Instant) -> Self {
            Self {
                now: Mutex::new(now),
                instant_reads: AtomicUsize::new(0),
            }
        }
    }

    impl Clock for SequentialClock {
        fn now(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH
        }

        fn now_instant(&self) -> Instant {
            self.instant_reads.fetch_add(1, Ordering::Relaxed);
            let mut now = self.now.lock().unwrap();
            let result = *now;
            *now += Duration::from_secs(1);
            result
        }
    }

    #[test]
    fn idle_to_provider_emits_only_provider_elapsed_time() {
        let (clock, mut tracker, intervals) = tracker("worker");
        clock.advance_mono(Duration::from_secs(10));
        tracker.enter_provider_wait();
        clock.advance_mono(Duration::from_secs(3));
        tracker.finish();
        assert_eq!(
            emitted(&intervals),
            vec![(
                SessionPhase::ProviderWait,
                SessionPhaseRole::Worker,
                Duration::from_secs(3)
            )]
        );
    }

    #[test]
    fn provider_to_tool_handoff_uses_one_clock_instant() {
        let (clock, mut tracker, intervals) = tracker("reviewer");
        tracker.enter_provider_wait();
        clock.advance_mono(Duration::from_secs(4));
        tracker.enter_tool_execution();
        clock.advance_mono(Duration::from_secs(6));
        tracker.finish();
        assert_eq!(
            emitted(&intervals),
            vec![
                (
                    SessionPhase::ProviderWait,
                    SessionPhaseRole::Reviewer,
                    Duration::from_secs(4)
                ),
                (
                    SessionPhase::ToolExecution,
                    SessionPhaseRole::Reviewer,
                    Duration::from_secs(6)
                ),
            ]
        );
    }

    #[test]
    fn direct_tool_to_provider_handoff_uses_one_clock_instant() {
        let (clock, mut tracker, intervals) = tracker("planner");
        tracker.enter_tool_execution();
        clock.advance_mono(Duration::from_secs(5));
        tracker.exit_tool_execution_to_provider_wait();
        clock.advance_mono(Duration::from_secs(7));
        tracker.finish();
        assert_eq!(
            emitted(&intervals),
            vec![
                (
                    SessionPhase::ToolExecution,
                    SessionPhaseRole::Planner,
                    Duration::from_secs(5)
                ),
                (
                    SessionPhase::ProviderWait,
                    SessionPhaseRole::Planner,
                    Duration::from_secs(7)
                ),
            ]
        );
    }

    #[test]
    fn direct_tool_to_provider_handoff_reads_the_clock_once() {
        let clock = Arc::new(SequentialClock::new(Instant::now()));
        let intervals = Arc::new(Mutex::new(Vec::new()));
        let mut tracker = SessionPhaseTracker {
            clock: clock.clone(),
            role: Some(SessionPhaseRole::Planner),
            active: None,
            tool_depth: 0,
            finished: false,
            emitted: intervals.clone(),
        };

        tracker.enter_tool_execution();
        tracker.exit_tool_execution_to_provider_wait();
        tracker.finish();

        assert_eq!(clock.instant_reads.load(Ordering::Relaxed), 3);
        assert_eq!(
            emitted(&intervals),
            vec![
                (
                    SessionPhase::ToolExecution,
                    SessionPhaseRole::Planner,
                    Duration::from_secs(1)
                ),
                (
                    SessionPhase::ProviderWait,
                    SessionPhaseRole::Planner,
                    Duration::from_secs(1)
                ),
            ]
        );
    }

    #[test]
    fn outer_tool_exit_excludes_local_work_before_provider_entry() {
        let (clock, mut tracker, intervals) = tracker("planner");
        tracker.enter_tool_execution();
        clock.advance_mono(Duration::from_secs(5));
        tracker.exit_tool_execution();
        clock.advance_mono(Duration::from_secs(2));
        tracker.enter_provider_wait();
        clock.advance_mono(Duration::from_secs(7));
        tracker.finish();
        assert_eq!(
            emitted(&intervals),
            vec![
                (
                    SessionPhase::ToolExecution,
                    SessionPhaseRole::Planner,
                    Duration::from_secs(5)
                ),
                (
                    SessionPhase::ProviderWait,
                    SessionPhaseRole::Planner,
                    Duration::from_secs(7)
                ),
            ]
        );
    }

    #[test]
    fn nested_tools_are_suppressed_into_one_interval() {
        let (clock, mut tracker, intervals) = tracker("refinement");
        tracker.enter_tool_execution();
        clock.advance_mono(Duration::from_secs(2));
        tracker.enter_tool_execution();
        clock.advance_mono(Duration::from_secs(3));
        tracker.exit_tool_execution();
        clock.advance_mono(Duration::from_secs(5));
        tracker.exit_tool_execution();
        tracker.finish();
        assert_eq!(
            emitted(&intervals),
            vec![(
                SessionPhase::ToolExecution,
                SessionPhaseRole::Refinement,
                Duration::from_secs(10)
            )]
        );
    }

    #[test]
    fn unknown_roles_are_suppressed() {
        let (clock, mut tracker, intervals) = tracker("architect");
        tracker.enter_provider_wait();
        clock.advance_mono(Duration::from_secs(4));
        tracker.enter_tool_execution();
        tracker.finish();
        assert!(emitted(&intervals).is_empty());
    }

    #[test]
    fn explicit_finish_flushes_once() {
        let (clock, mut tracker, intervals) = tracker("worker");
        tracker.enter_provider_wait();
        clock.advance_mono(Duration::from_secs(8));
        tracker.finish();
        clock.advance_mono(Duration::from_secs(8));
        tracker.finish();
        assert_eq!(
            emitted(&intervals),
            vec![(
                SessionPhase::ProviderWait,
                SessionPhaseRole::Worker,
                Duration::from_secs(8)
            )]
        );
    }

    #[test]
    fn drop_flushes_an_active_interval_once() {
        let (clock, mut tracker, intervals) = tracker("worker");
        tracker.enter_provider_wait();
        clock.advance_mono(Duration::from_secs(9));
        drop(tracker);
        assert_eq!(
            emitted(&intervals),
            vec![(
                SessionPhase::ProviderWait,
                SessionPhaseRole::Worker,
                Duration::from_secs(9)
            )]
        );
    }

    #[test]
    fn drop_after_finish_does_not_flush_again() {
        let (clock, mut tracker, intervals) = tracker("worker");
        tracker.enter_provider_wait();
        clock.advance_mono(Duration::from_secs(9));
        tracker.finish();
        drop(tracker);
        assert_eq!(
            emitted(&intervals),
            vec![(
                SessionPhase::ProviderWait,
                SessionPhaseRole::Worker,
                Duration::from_secs(9)
            )]
        );
    }
}

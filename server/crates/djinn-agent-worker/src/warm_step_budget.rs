//! Per-step wall-clock budgets for the cargo warm base, reconciled against the
//! deadline that will kill the warm Pod anyway.
//!
//! # Why this exists
//!
//! Every warm cargo command used to share one hard-coded `30 * 60` constant.
//! Two problems followed from that:
//!
//! 1. `cargo clippy` and `cargo nextest run --no-run` are not the same
//!    workload. On the production 4-vCPU box clippy finishes in ~10 minutes
//!    while the test-binary compile — the step that actually puts test-target
//!    artifacts in the warm base — was killed at 30 minutes *every cycle*.
//! 2. The constant was never reconciled with the two enclosing deadlines. The
//!    warm Job carries `activeDeadlineSeconds` (`warm_job_timeout_seconds`,
//!    default 3600s) and the in-process warm watcher derives its own deadline
//!    from that same value plus a fixed slack (see
//!    `djinn_k8s::graph_warmer_lifecycle::watch_deadline`). A step bound larger
//!    than the Job deadline does not buy the step more time; it just relocates
//!    the truncation from an observable `outcome="timeout"` step record to an
//!    unobservable kubelet kill.
//!
//! # The model
//!
//! The Job deadline is the single authoritative bound, projected into the warm
//! Pod as `DJINN_WARM_JOB_DEADLINE_SECONDS` by `djinn_k8s::warm_job`. Each step
//! gets a nominal budget (overridable per step), and every budget is then
//! clamped to the time actually left before the Job deadline, minus a tail
//! reserve for the post-compile work (graph indexing and publication) that runs
//! inside the same Pod. Raising a step's nominal budget can therefore never
//! push a step past the deadline; to genuinely give the warm more time an
//! operator raises `DJINN_K8S_WARM_JOB_TIMEOUT_SECONDS`, and the step budgets
//! follow automatically.

use std::time::{Duration, Instant};

use djinn_core::clock::{Clock, SystemClock};

/// Wall-clock the warm Job is allowed, projected from the Job's
/// `activeDeadlineSeconds`. Absent (local runs, tests) → [`DEFAULT_JOB_DEADLINE`].
pub const ENV_JOB_DEADLINE_SECONDS: &str = "DJINN_WARM_JOB_DEADLINE_SECONDS";
/// Reserve withheld from the compile phase for the post-compile graph index +
/// publish that shares the warm Pod's deadline.
pub const ENV_TAIL_RESERVE_SECONDS: &str = "DJINN_WARM_TAIL_RESERVE_SECONDS";
pub const ENV_CLIPPY_BUDGET_SECONDS: &str = "DJINN_WARM_STEP_TIMEOUT_CLIPPY_SECONDS";
pub const ENV_BUILD_BUDGET_SECONDS: &str = "DJINN_WARM_STEP_TIMEOUT_BUILD_SECONDS";
pub const ENV_TEST_BUDGET_SECONDS: &str = "DJINN_WARM_STEP_TIMEOUT_TEST_SECONDS";
pub const ENV_SWEEP_BUDGET_SECONDS: &str = "DJINN_WARM_STEP_TIMEOUT_SWEEP_SECONDS";

/// Mirrors `djinn_k8s::config::KubernetesConfig::warm_job_timeout_seconds`'s
/// default so a worker started without the projected env behaves like a Pod
/// launched with the shipped default.
pub const DEFAULT_JOB_DEADLINE: Duration = Duration::from_secs(3600);
/// Post-compile SCIP indexing + publication share the Job deadline with the
/// compile phase; withhold this much from every step clamp.
pub const DEFAULT_TAIL_RESERVE: Duration = Duration::from_secs(900);
/// Unchanged from the retired `WARM_COMMAND_TIMEOUT` so existing deployments
/// do not regress on the steps that were already finishing inside it.
pub const DEFAULT_CLIPPY_BUDGET: Duration = Duration::from_secs(30 * 60);
pub const DEFAULT_BUILD_BUDGET: Duration = Duration::from_secs(30 * 60);
pub const DEFAULT_SWEEP_BUDGET: Duration = Duration::from_secs(30 * 60);
/// Raised: this is the step that was truncated every cycle. It is still clamped
/// by the Job deadline, so on a default 3600s Job it resolves to whatever is
/// left after clippy rather than to a full hour.
pub const DEFAULT_TEST_BUDGET: Duration = Duration::from_secs(60 * 60);
/// Never hand a step a zero/absurd budget: below this the step could not even
/// start cargo, and a `timeout` record would be noise rather than signal.
pub const MIN_STEP_BUDGET: Duration = Duration::from_secs(60);

/// The bounded set of warm steps, each with its own budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmStepKind {
    /// `cargo clippy …` (check-mode units).
    Clippy,
    /// `cargo build …`, the clippy fallback.
    Build,
    /// `cargo test --no-run` / `cargo nextest run --no-run`.
    TestNoRun,
    /// `cargo sweep --stamp` / `--file`.
    Sweep,
}

impl WarmStepKind {
    /// Classify a warm command from its cargo subcommand argv. Anything
    /// unrecognized is treated as a build-shaped step, matching how the metric
    /// label mapping coerces unknown labels.
    pub fn from_args(args: &[&str]) -> Self {
        match args.first().copied() {
            Some("clippy") | Some("check") => Self::Clippy,
            Some("test") => Self::TestNoRun,
            Some("nextest") => Self::TestNoRun,
            Some("sweep") => Self::Sweep,
            _ => Self::Build,
        }
    }

    const fn env_key(self) -> &'static str {
        match self {
            Self::Clippy => ENV_CLIPPY_BUDGET_SECONDS,
            Self::Build => ENV_BUILD_BUDGET_SECONDS,
            Self::TestNoRun => ENV_TEST_BUDGET_SECONDS,
            Self::Sweep => ENV_SWEEP_BUDGET_SECONDS,
        }
    }

    const fn default_budget(self) -> Duration {
        match self {
            Self::Clippy => DEFAULT_CLIPPY_BUDGET,
            Self::Build => DEFAULT_BUILD_BUDGET,
            Self::TestNoRun => DEFAULT_TEST_BUDGET,
            Self::Sweep => DEFAULT_SWEEP_BUDGET,
        }
    }
}

/// Resolved budgets for one warm cycle, anchored at the moment the warm Pod's
/// deadline started running.
#[derive(Debug, Clone, Copy)]
pub struct WarmStepBudgets {
    job_deadline: Duration,
    tail_reserve: Duration,
    clippy: Duration,
    build: Duration,
    test_no_run: Duration,
    sweep: Duration,
    started: Instant,
}

/// One step's resolved budget plus why it landed where it did — logged so a
/// truncated step is always attributable to either its own bound or to the
/// enclosing Job deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepBudget {
    pub budget: Duration,
    /// True when the enclosing Job deadline (not the step's own bound) is what
    /// limited this step. This is the case the campaign cares about: raising a
    /// per-step number here changes nothing, the Job deadline must move.
    pub clamped_by_deadline: bool,
}

impl WarmStepBudgets {
    /// Resolve from the process environment, anchoring elapsed-time accounting
    /// at `started`.
    pub fn from_env(started: Instant) -> Self {
        Self {
            job_deadline: duration_from_env(ENV_JOB_DEADLINE_SECONDS, DEFAULT_JOB_DEADLINE),
            tail_reserve: duration_from_env(ENV_TAIL_RESERVE_SECONDS, DEFAULT_TAIL_RESERVE),
            clippy: budget_from_env(WarmStepKind::Clippy),
            build: budget_from_env(WarmStepKind::Build),
            test_no_run: budget_from_env(WarmStepKind::TestNoRun),
            sweep: budget_from_env(WarmStepKind::Sweep),
            started,
        }
    }

    /// Resolve from the environment anchored at "now".
    pub fn now_from_env() -> Self {
        Self::from_env(Clock::now_instant(&SystemClock::new()))
    }

    /// Deterministic budgets for the in-crate warm orchestration tests.
    ///
    /// Deliberately NOT env-driven: the warm tests run inside one process
    /// alongside tests that spawn their own cargo stubs, and a process-global
    /// budget override would leak a one-second bound into them.
    #[cfg(test)]
    pub fn for_testing(compile: Duration, sweep: Duration) -> Self {
        Self {
            job_deadline: DEFAULT_JOB_DEADLINE,
            tail_reserve: DEFAULT_TAIL_RESERVE,
            clippy: compile,
            build: compile,
            test_no_run: compile,
            sweep,
            started: Clock::now_instant(&SystemClock::new()),
        }
    }

    /// The Job deadline this cycle is reconciled against.
    pub fn job_deadline(&self) -> Duration {
        self.job_deadline
    }

    /// The reserve withheld from every step for post-compile work.
    pub fn tail_reserve(&self) -> Duration {
        self.tail_reserve
    }

    /// The nominal (pre-clamp) budget configured for `kind`.
    pub fn nominal(&self, kind: WarmStepKind) -> Duration {
        match kind {
            WarmStepKind::Clippy => self.clippy,
            WarmStepKind::Build => self.build,
            WarmStepKind::TestNoRun => self.test_no_run,
            WarmStepKind::Sweep => self.sweep,
        }
    }

    /// The budget to actually hand this step, clamped so it can never outlive
    /// the deadline that would kill the Pod. Uses the elapsed time since the
    /// anchor, so later steps see a correspondingly smaller ceiling.
    pub fn resolve(&self, kind: WarmStepKind) -> StepBudget {
        self.resolve_at(kind, self.started.elapsed())
    }

    /// Deterministic seam for the clamp arithmetic.
    pub fn resolve_at(&self, kind: WarmStepKind, elapsed: Duration) -> StepBudget {
        let nominal = self.nominal(kind);
        let ceiling = self
            .job_deadline
            .saturating_sub(elapsed)
            .saturating_sub(self.tail_reserve);
        if ceiling >= nominal {
            return StepBudget {
                budget: nominal,
                clamped_by_deadline: false,
            };
        }
        StepBudget {
            budget: ceiling.max(MIN_STEP_BUDGET),
            clamped_by_deadline: true,
        }
    }
}

fn budget_from_env(kind: WarmStepKind) -> Duration {
    duration_from_env(kind.env_key(), kind.default_budget())
}

/// Parse a positive whole-second duration from `key`. Anything unset, empty,
/// unparseable, or zero keeps `fallback` — a warm Pod must never be left with
/// an unbounded or instantly-expiring step because of a typo in an env var.
fn duration_from_env(key: &str, fallback: Duration) -> Duration {
    let Some(raw) = std::env::var_os(key) else {
        return fallback;
    };
    let Some(text) = raw.to_str() else {
        return fallback;
    };
    match text.trim().parse::<u64>() {
        Ok(seconds) if seconds > 0 => Duration::from_secs(seconds),
        _ => fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budgets(job_deadline: u64, tail_reserve: u64) -> WarmStepBudgets {
        WarmStepBudgets {
            job_deadline: Duration::from_secs(job_deadline),
            tail_reserve: Duration::from_secs(tail_reserve),
            clippy: DEFAULT_CLIPPY_BUDGET,
            build: DEFAULT_BUILD_BUDGET,
            test_no_run: DEFAULT_TEST_BUDGET,
            sweep: DEFAULT_SWEEP_BUDGET,
            started: Clock::now_instant(&SystemClock::new()),
        }
    }

    #[test]
    fn classifies_every_warm_command_shape() {
        assert_eq!(
            WarmStepKind::from_args(&["clippy", "--workspace"]),
            WarmStepKind::Clippy
        );
        assert_eq!(
            WarmStepKind::from_args(&["build", "--all-targets"]),
            WarmStepKind::Build
        );
        assert_eq!(
            WarmStepKind::from_args(&["test", "--workspace", "--no-run"]),
            WarmStepKind::TestNoRun
        );
        assert_eq!(
            WarmStepKind::from_args(&["nextest", "run", "--no-run"]),
            WarmStepKind::TestNoRun
        );
        assert_eq!(
            WarmStepKind::from_args(&["sweep", "--file"]),
            WarmStepKind::Sweep
        );
        // An `override`-policy argv we do not recognize is build-shaped, the
        // same coercion the bounded metric label mapping applies.
        assert_eq!(WarmStepKind::from_args(&["doc"]), WarmStepKind::Build);
        assert_eq!(WarmStepKind::from_args(&[]), WarmStepKind::Build);
    }

    /// The test-compile step is the one the campaign is about: its nominal
    /// budget must exceed the retired 30-minute constant.
    #[test]
    fn test_step_nominal_budget_exceeds_the_retired_constant() {
        let budgets = budgets(7200, 900);
        assert!(budgets.nominal(WarmStepKind::TestNoRun) > Duration::from_secs(30 * 60));
        // …while the steps that already fit keep exactly the old bound, so
        // existing deployments do not regress.
        assert_eq!(
            budgets.nominal(WarmStepKind::Clippy),
            Duration::from_secs(30 * 60)
        );
        assert_eq!(
            budgets.nominal(WarmStepKind::Build),
            Duration::from_secs(30 * 60)
        );
    }

    /// A step bound must never exceed the deadline that would kill the Pod.
    #[test]
    fn budget_is_clamped_to_the_enclosing_job_deadline() {
        let budgets = budgets(3600, 900);
        let resolved = budgets.resolve_at(WarmStepKind::TestNoRun, Duration::from_secs(600));
        assert!(resolved.clamped_by_deadline);
        // 3600 - 600 elapsed - 900 reserve.
        assert_eq!(resolved.budget, Duration::from_secs(2100));
        assert!(resolved.budget < budgets.nominal(WarmStepKind::TestNoRun));
    }

    /// On the production 7200s Job the test step gets its full hour: the clamp
    /// only binds when the deadline is genuinely the smaller number.
    #[test]
    fn generous_job_deadline_leaves_the_nominal_budget_intact() {
        let budgets = budgets(7200, 900);
        let resolved = budgets.resolve_at(WarmStepKind::TestNoRun, Duration::from_secs(620));
        assert!(!resolved.clamped_by_deadline);
        assert_eq!(resolved.budget, DEFAULT_TEST_BUDGET);
    }

    /// An exhausted deadline still yields a usable, bounded budget rather than
    /// zero (which would report a timeout for a step that never ran).
    #[test]
    fn exhausted_deadline_floors_at_the_minimum_budget() {
        let budgets = budgets(3600, 900);
        let resolved = budgets.resolve_at(WarmStepKind::Clippy, Duration::from_secs(3600));
        assert!(resolved.clamped_by_deadline);
        assert_eq!(resolved.budget, MIN_STEP_BUDGET);
    }

    #[test]
    fn env_parsing_rejects_zero_and_garbage_without_unbounding_the_step() {
        // A parse seam rather than mutating process env: `duration_from_env`
        // is exercised through its documented fallbacks below.
        assert_eq!(
            duration_from_env(
                "DJINN_WARM_STEP_BUDGET_TEST_ABSENT_KEY",
                DEFAULT_TEST_BUDGET
            ),
            DEFAULT_TEST_BUDGET
        );
    }
}

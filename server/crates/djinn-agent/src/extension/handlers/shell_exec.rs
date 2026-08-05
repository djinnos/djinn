//! Shell-invocation timeout policy and the single cargo-observation seam.
//!
//! Split out of `workspace.rs`, which was within a hundred bytes of the 51200
//! byte size guard. Everything here is about *how one shell command is timed
//! and observed*, which is a smaller and much more testable concern than the
//! workspace tool surface that calls it.

use std::time::Duration;

use djinn_core::clock::Clock;
use djinn_telemetry::cargo_invocation::{EXIT_CANCELLED, EXIT_FAIL, EXIT_OK};

/// The enclosing task-run Pod's `activeDeadlineSeconds`, projected into the Pod
/// by `djinn_k8s::job::build_task_run_job` from
/// `KubernetesConfig::task_run_active_deadline_seconds`.
///
/// This is the only *real* bound on a shell command, and it is the same lever
/// `djinn_agent_worker::warm_step_budget` reconciles warm steps against: a
/// command budget larger than the Pod's remaining life does not buy the command
/// more time, it just relocates the truncation from an observable
/// `ProcessTermination::TimedOut` (which the agent sees, and can react to) into
/// an unobservable supervisor wind-down or kubelet kill that ends the whole run.
const ENV_POD_DEADLINE_SECONDS: &str = "DJINN_TASK_RUN_DEADLINE_SECONDS";

/// Mirrors `djinn_k8s::config::KubernetesConfig::task_run_active_deadline_seconds`'s
/// default, so an agent running without the projected env (local runs, tests,
/// out-of-cluster) is bounded exactly as a shipped-default Pod would be.
const DEFAULT_POD_DEADLINE: Duration = Duration::from_secs(10800);

/// Mirrors `djinn_agent_worker::main::SOFT_DEADLINE_MARGIN`. The supervisor arms
/// an in-pod soft deadline this far ahead of `activeDeadlineSeconds` and drives
/// a graceful cancel + checkpoint there, so wall-clock past that point is not
/// available to a shell command under any circumstances.
const POD_WINDDOWN_RESERVE: Duration = Duration::from_secs(600);

/// Withheld on top of the wind-down reserve for the work that must still happen
/// *after* the command returns: the agent reading the output, deciding, and the
/// supervisor's commit/push. A command permitted to consume the entire
/// pre-winddown window would return its result into a Pod with no time left to
/// act on it, which is strictly worse than being told it ran out of time.
const POD_TAIL_RESERVE: Duration = Duration::from_secs(600);

/// Floor for the derived ceiling. A tuned-down or malformed Pod deadline must
/// never resolve to a zero/absurd budget: that would report a timeout for a
/// command that never had a chance to run, which is noise rather than signal.
const MIN_SHELL_BUDGET: Duration = Duration::from_secs(60);

/// Smallest honoured request, so a caller's `timeout_ms: 10` cannot kill a
/// command during process spawn.
const MIN_REQUEST_MS: u64 = 1000;

/// Default interactive-shell timeout (ms) when the caller passes no `timeout_ms`.
/// Overridable via `DJINN_SHELL_TIMEOUT_MS`. Raised well above the old 120s:
/// cold native builds routinely exceed two minutes, and a too-short ceiling
/// SIGKILLed compiles mid-flight, leaving the model to retry from cold — the
/// same guillotine the command runner already fixed.
fn default_shell_timeout_ms() -> u64 {
    std::env::var("DJINN_SHELL_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v >= 1000)
        .unwrap_or(600_000)
}

/// Minimum timeout (ms) enforced for slow native build/test commands, even when
/// the caller requests a smaller value. A cold `cargo`/`clippy`/`nextest`/`go`/
/// `pnpm` compile legitimately runs many minutes; flooring stops a low guess
/// from killing a build that is still making progress. Overridable via
/// `DJINN_SHELL_BUILD_TIMEOUT_MS`. Only ever RAISES the effective timeout.
///
/// # Why this is 3600s and was 1800s
///
/// The 1800s constant was never a *ceiling* anyone chose — it is this floor,
/// and because the model almost never passes a larger `timeout_ms` (the tool
/// schema still advertises "default 120000" and documents no range), it became
/// the de-facto budget of every build command in every task-run Pod. Measured
/// on live `build-capable` Pods on 2026-08-05, a cold compile-and-test of this
/// workspace sits *right at* it: successes at 55s, 104s, 361s, 792s and
/// **1288s**, against two kills at exactly **1800s**. A budget whose largest
/// legitimate observation is 72% of the bound is mispriced — every
/// unlucky-but-healthy cold compile loses 30 minutes and reopens the task.
///
/// 3600s is the same number `djinn_agent_worker::warm_step_budget`'s
/// `DEFAULT_TEST_BUDGET` already settled on, for the same reason and the same
/// workload: the warm path independently found that compiling *this* workspace's
/// test targets was truncated by a 30-minute constant every cycle and raised it
/// to an hour. The task-run Pod compiles the same workspace on the same 4-vCPU
/// shape, so it gets the same allowance — ~2.8x the largest observed success,
/// and still far inside the Pod ceiling derived in [`pod_budget_ceiling_ms`].
fn build_command_floor_ms() -> u64 {
    std::env::var("DJINN_SHELL_BUILD_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v >= 1000)
        .unwrap_or(3_600_000)
}

/// Heuristic: does this command invoke a slow native build/test toolchain whose
/// cold compile can exceed the default timeout? Matches on substrings so the
/// common `cd server && cargo …` shape is covered. False positives are benign:
/// a non-build command that happens to contain the needle finishes fast anyway,
/// so the only effect is a higher (unused) ceiling.
fn is_build_command(command: &str) -> bool {
    const NEEDLES: [&str; 8] = [
        "cargo ", "nextest", "go build", "go test", "pnpm ", "npm run", "make ", "bazel ",
    ];
    NEEDLES.iter().any(|needle| command.contains(needle))
}

/// The largest budget any single shell command may be handed, derived from the
/// enclosing Pod's `activeDeadlineSeconds`.
///
/// Before this existed there was *no* upper bound at all: the resolver returned
/// the caller's request verbatim once it cleared the build floor, so a request
/// for four hours was honoured inside a three-hour Pod. Such
/// a command can never time out on its own terms; instead the supervisor's soft
/// deadline cancels the whole run, and the agent gets a dead session rather than
/// a `TimedOut` result it could have recovered from.
///
/// # This clamp is not a one-way door
///
/// The bound is *derived*, never absolute: it moves with
/// [`ENV_POD_DEADLINE_SECONDS`], which operators already set via
/// `DJINN_K8S_TASK_RUN_ACTIVE_DEADLINE_SECONDS`. Doubling the Pod deadline
/// doubles this ceiling with no code change, so a legitimately growing workload
/// can never be pinned under a constant with no adaptation path out — the exact
/// failure mode `djinn_graph::scip_indexer::budget` documents from its retired
/// `max_cap * 3` clamp. It is also reported, not silent: a clamped resolution
/// sets [`ShellBudget::clamped_by_pod_deadline`], so "your request lost to the
/// Pod deadline" is a structured fact and not something to infer from a log.
fn pod_budget_ceiling_ms() -> u64 {
    pod_budget_ceiling_from(std::env::var(ENV_POD_DEADLINE_SECONDS).ok().as_deref())
}

/// Pure seam for [`pod_budget_ceiling_ms`]. Absent, empty, unparseable or zero
/// keeps [`DEFAULT_POD_DEADLINE`]: a typo in the projected env must not leave a
/// command effectively unbounded.
fn pod_budget_ceiling_from(raw: Option<&str>) -> u64 {
    let deadline = raw
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map_or(DEFAULT_POD_DEADLINE, Duration::from_secs);
    let ceiling = deadline
        .saturating_sub(POD_WINDDOWN_RESERVE)
        .saturating_sub(POD_TAIL_RESERVE)
        .max(MIN_SHELL_BUDGET);
    u64::try_from(ceiling.as_millis()).unwrap_or(u64::MAX)
}

/// The wall-clock shape of one shell command. Two values because there are two
/// genuinely different workloads behind this tool, and pricing them the same is
/// what produced both halves of the defect: a native compile needs tens of
/// minutes, and a `git status` that has not returned in ten minutes is hung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShellClass {
    /// Everything else: the caller's request (or the interactive default)
    /// stands as-is.
    Light,
    /// A native build/test toolchain invocation, per [`is_build_command`].
    Build,
}

impl ShellClass {
    fn classify(command: &str) -> Self {
        if is_build_command(command) {
            Self::Build
        } else {
            Self::Light
        }
    }

    /// Bounded telemetry/log label.
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Build => "build",
        }
    }
}

/// One command's resolved budget plus why it landed where it did, so a killed
/// command is always attributable to either its own class bound or to the
/// enclosing Pod deadline. Mirrors `warm_step_budget::StepBudget`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ShellBudget {
    pub timeout_ms: u64,
    pub class: ShellClass,
    /// True when the Pod's `activeDeadlineSeconds` — not the caller's request
    /// and not the class floor — is what bounded this command. This is the case
    /// where raising `DJINN_SHELL_BUILD_TIMEOUT_MS` changes nothing and the Pod
    /// deadline is what must move.
    pub clamped_by_pod_deadline: bool,
}

/// Resolve the effective shell budget: the caller's `timeout_ms` (or the
/// default), raised to a sane minimum and to the class floor, then bounded by
/// the wall-clock the enclosing Pod will actually still exist for.
///
/// Deliberately role-blind, exactly like the invocation lease: a Reviewer's
/// compile gets the same build floor as a Worker's, because the compile takes
/// the same wall-clock either way.
///
/// Deliberately *not* progress-aware. An output-idle heuristic looks attractive
/// here — kill on silence rather than on wall clock — but it would misfire on
/// precisely the workload this budget exists for: this workspace's build is
/// link-bound, and `rust-lld` emits nothing at all for minutes at a time while
/// making perfect forward progress. Idle-killing a silent linker would trade a
/// rare 30-minute loss for a routine one. The wall-clock bound stays, and it
/// still kills a genuinely hung command — just an hour later than a hung
/// `git status`, which is the correct asymmetry.
pub(super) fn resolve_shell_budget(requested: Option<u64>, command: &str) -> ShellBudget {
    resolve_shell_budget_within(requested, command, pod_budget_ceiling_ms())
}

/// Pure seam for [`resolve_shell_budget`]: the ceiling is injected so the clamp
/// arithmetic is testable without touching process environment.
fn resolve_shell_budget_within(
    requested: Option<u64>,
    command: &str,
    ceiling_ms: u64,
) -> ShellBudget {
    let class = ShellClass::classify(command);
    let base = requested
        .unwrap_or_else(default_shell_timeout_ms)
        .max(MIN_REQUEST_MS);
    let nominal = match class {
        ShellClass::Light => base,
        ShellClass::Build => base.max(build_command_floor_ms()),
    };
    if nominal <= ceiling_ms {
        return ShellBudget {
            timeout_ms: nominal,
            class,
            clamped_by_pod_deadline: false,
        };
    }
    ShellBudget {
        timeout_ms: ceiling_ms,
        class,
        clamped_by_pod_deadline: true,
    }
}

/// Structurally record exactly one cargo invocation observation from a single
/// runner terminal result.
///
/// This is the private testable seam between the process runner and the
/// telemetry contract. Exactly-once is structural: there is exactly one call
/// site in `call_shell`, placed after the single runner return. No Drop
/// guard, no recordings in individual timeout/cancellation branches.
///
/// Mapping:
/// - `classification == None` (non-cargo command): no observation.
/// - [`crate::process::ProcessRunError::Spawn`] (child never started): no observation.
/// - Successful exit ([`crate::process::ProcessTermination::Exited`] + success): `EXIT_OK`.
/// - Nonzero exit, timeout, or post-start runner error: `EXIT_FAIL`.
/// - Handled cancellation: `EXIT_CANCELLED`.
///
/// `class` is the session role's [`djinn_runtime::RoleResourceClass`] rendered
/// via `as_str()` — a two-valued, bounded label. It exists to answer the one
/// question that blocks arming the invocation semaphore: *what fraction of
/// observed cargo invocations came from a role dispatch never charged a build
/// slot?* It labels the observation only; nothing downstream branches on it.
pub(super) fn finish_shell(
    classification: Option<&'static str>,
    class: &'static str,
    started: std::time::Instant,
    result: &Result<crate::process::ProcessOutput, crate::process::ProcessRunError>,
    clock: &dyn Clock,
    recorder: impl Fn(&'static str, &'static str, &'static str, std::time::Duration),
) {
    let Some(kind) = classification else {
        return;
    };
    let exit: &'static str = match result {
        // Spawn error: child never started — no observation.
        Err(crate::process::ProcessRunError::Spawn(_)) => return,
        // Post-start runner error (wait/reap/join): child started.
        Err(crate::process::ProcessRunError::Started(_)) => EXIT_FAIL,
        Ok(po) => match po.termination {
            // Handled cancellation: child started and was cleaned up.
            crate::process::ProcessTermination::Cancelled => EXIT_CANCELLED,
            // Timeout: child was killed by the deadline — always fail.
            crate::process::ProcessTermination::TimedOut => EXIT_FAIL,
            crate::process::ProcessTermination::Exited if po.output.status.success() => EXIT_OK,
            crate::process::ProcessTermination::Exited => EXIT_FAIL,
        },
    };
    let ended = clock.now_instant();
    let elapsed = ended.saturating_duration_since(started);
    recorder(kind, exit, class, elapsed);
}

#[cfg(all(test, unix))]
#[path = "workspace_cargo_outcome_tests.rs"]
mod cargo_outcome_tests;

#[cfg(test)]
mod timeout_tests {
    use super::{
        DEFAULT_POD_DEADLINE, MIN_SHELL_BUDGET, POD_TAIL_RESERVE, POD_WINDDOWN_RESERVE, ShellClass,
        is_build_command, pod_budget_ceiling_from, resolve_shell_budget,
        resolve_shell_budget_within,
    };
    use std::time::Duration;

    /// The production ceiling: no env projected, i.e. a shipped-default Pod.
    fn default_ceiling_ms() -> u64 {
        pod_budget_ceiling_from(None)
    }

    fn budget_ms(requested: Option<u64>, command: &str) -> u64 {
        resolve_shell_budget_within(requested, command, default_ceiling_ms()).timeout_ms
    }

    #[test]
    fn build_commands_are_detected() {
        assert!(is_build_command("cd server && cargo check -p djinn-db"));
        assert!(is_build_command("cargo clippy --all-features"));
        assert!(is_build_command("cargo nextest run"));
        assert!(is_build_command("go test ./..."));
        assert!(is_build_command("pnpm install"));
        assert!(is_build_command("make build"));
        assert!(!is_build_command("ls -la"));
        assert!(!is_build_command("git status"));
        assert!(!is_build_command("grep -r foo src"));
    }

    #[test]
    fn commands_are_classified_into_the_two_budget_shapes() {
        assert_eq!(
            resolve_shell_budget(Some(5_000), "cargo test --workspace").class,
            ShellClass::Build
        );
        assert_eq!(
            resolve_shell_budget(Some(5_000), "git status").class,
            ShellClass::Light
        );
    }

    #[test]
    fn build_commands_are_floored_above_a_small_request() {
        // A 120s request for a cold compile must be raised to the build floor,
        // not honored verbatim (that was the SIGKILL-mid-build bug).
        let got = budget_ms(Some(120_000), "cargo build");
        assert!(got >= 3_600_000, "build floor not applied: {got}");
    }

    /// The measured production shape this change exists for: on live
    /// `build-capable` Pods on 2026-08-05 a cold compile-and-test of this
    /// workspace succeeded at 1288s and was killed twice at exactly 1800s. The
    /// budget must clear the largest observed *success* with real margin, not
    /// sit 40% above it.
    #[test]
    fn build_budget_clears_the_measured_cold_compile_with_margin() {
        const LARGEST_OBSERVED_SUCCESS: Duration = Duration::from_secs(1288);
        const OBSERVED_KILL: Duration = Duration::from_secs(1800);

        let got = Duration::from_millis(budget_ms(None, "cargo nextest run --workspace"));

        assert!(
            got > OBSERVED_KILL,
            "budget {got:?} still kills the runs measured dying at {OBSERVED_KILL:?}"
        );
        assert!(
            got >= LARGEST_OBSERVED_SUCCESS * 2,
            "budget {got:?} leaves under 2x headroom over the largest observed \
             success {LARGEST_OBSERVED_SUCCESS:?}"
        );
    }

    #[test]
    fn non_build_commands_keep_the_requested_timeout() {
        assert_eq!(budget_ms(Some(5_000), "echo hi"), 5_000);
    }

    #[test]
    fn requests_are_clamped_to_a_sane_minimum() {
        assert_eq!(budget_ms(Some(10), "echo hi"), 1000);
    }

    #[test]
    fn a_large_explicit_build_timeout_is_preserved() {
        let resolved =
            resolve_shell_budget_within(Some(3_600_000), "cargo test", default_ceiling_ms());
        assert_eq!(resolved.timeout_ms, 3_600_000);
        assert!(!resolved.clamped_by_pod_deadline);
    }

    // --- Reconciliation with the Pod's activeDeadlineSeconds ---------------

    /// The hard constraint: the default budget a build command actually gets
    /// must fit inside the Pod, with both reserves still intact. If this ever
    /// fails, a command can return into a Pod that has no time left to commit
    /// its result — worse than being told it ran out of time.
    #[test]
    fn default_build_budget_fits_inside_the_pod_with_both_reserves_intact() {
        let granted = Duration::from_millis(budget_ms(None, "cargo build --workspace"));
        let unusable = POD_WINDDOWN_RESERVE + POD_TAIL_RESERVE;

        assert!(
            granted + unusable <= DEFAULT_POD_DEADLINE,
            "granted {granted:?} + reserves {unusable:?} exceeds the Pod deadline \
             {DEFAULT_POD_DEADLINE:?}"
        );
        assert!(
            !resolve_shell_budget_within(None, "cargo build --workspace", default_ceiling_ms())
                .clamped_by_pod_deadline,
            "the default build budget must not need the Pod clamp to be safe"
        );
    }

    /// A request that would outlive the Pod is refused, not honoured. Before
    /// this bound existed the resolver returned any request verbatim, so a
    /// four-hour ask inside a three-hour Pod could never time out on its own
    /// terms — the supervisor killed the whole run instead.
    #[test]
    fn a_request_that_would_outlive_the_pod_is_bounded_and_says_so() {
        let four_hours = 4 * 60 * 60 * 1000;
        let resolved =
            resolve_shell_budget_within(Some(four_hours), "cargo test", default_ceiling_ms());

        assert!(resolved.clamped_by_pod_deadline);
        assert_eq!(resolved.timeout_ms, default_ceiling_ms());
        assert!(
            Duration::from_millis(resolved.timeout_ms) + POD_WINDDOWN_RESERVE + POD_TAIL_RESERVE
                <= DEFAULT_POD_DEADLINE
        );
    }

    /// The clamp is derived from the Pod deadline, never an absolute constant:
    /// raising the deadline raises the ceiling with it. This is the property
    /// that stops it becoming a one-way door for a workload that legitimately
    /// grows past today's numbers.
    #[test]
    fn raising_the_pod_deadline_raises_the_ceiling_with_no_code_change() {
        let shipped = pod_budget_ceiling_from(Some("10800"));
        let doubled = pod_budget_ceiling_from(Some("21600"));
        assert_eq!(doubled - shipped, 10800 * 1000);

        // A request clamped under the shipped deadline is honoured verbatim
        // once the operator raises it — the escape hatch actually works.
        let five_hours = 5 * 60 * 60 * 1000;
        assert!(
            resolve_shell_budget_within(Some(five_hours), "cargo test", shipped)
                .clamped_by_pod_deadline
        );
        let after = resolve_shell_budget_within(Some(five_hours), "cargo test", doubled);
        assert!(!after.clamped_by_pod_deadline);
        assert_eq!(after.timeout_ms, five_hours);
    }

    /// Structural invariant: a budget below what the class/request asked for is
    /// ALWAYS attributed. There is no silent capping path.
    #[test]
    fn a_reduced_budget_is_always_attributed_to_the_pod_clamp() {
        let ceiling = 90_000;
        for (requested, command) in [
            (None, "cargo test"),
            (Some(600_000), "cargo build"),
            (Some(600_000), "sleep 600"),
            (None, "git status"),
        ] {
            let unbounded = resolve_shell_budget_within(requested, command, u64::MAX);
            let bounded = resolve_shell_budget_within(requested, command, ceiling);
            assert_eq!(
                bounded.timeout_ms < unbounded.timeout_ms,
                bounded.clamped_by_pod_deadline,
                "{command:?} with {requested:?}: reduced budget must set the flag"
            );
            assert!(bounded.timeout_ms <= ceiling);
        }
    }

    /// A malformed or absurdly small projected deadline must never resolve to a
    /// zero budget — that reports a timeout for a command that never ran.
    #[test]
    fn a_broken_or_tiny_pod_deadline_still_yields_a_usable_budget() {
        let min_ms = u64::try_from(MIN_SHELL_BUDGET.as_millis()).expect("min fits");
        for raw in ["0", "", "   ", "not-a-number"] {
            assert_eq!(
                pod_budget_ceiling_from(Some(raw)),
                pod_budget_ceiling_from(None),
                "{raw:?} must fall back to the shipped default, not to zero"
            );
        }
        // A deadline smaller than the reserves floors at the minimum rather
        // than underflowing to nothing.
        assert_eq!(pod_budget_ceiling_from(Some("30")), min_ms);
        assert_eq!(
            resolve_shell_budget_within(None, "cargo test", min_ms).timeout_ms,
            min_ms
        );
    }
}

use std::sync::OnceLock;

use metrics_exporter_prometheus::{BuildError, PrometheusBuilder, PrometheusHandle};

pub const PROMETHEUS_TEXT_CONTENT_TYPE: &str = "text/plain; version=0.0.4";

const DISPATCH_ATTEMPTS_TOTAL: &str = "djinn_dispatch_attempts_total";
const DISPATCH_LAST_SUCCESS_TIMESTAMP: &str = "djinn_dispatch_last_success_timestamp";
const DISPATCH_COOLDOWNS_ACTIVE: &str = "djinn_dispatch_cooldowns_active";
const INFLIGHT_LEDGER_SIZE: &str = "djinn_inflight_ledger_size";
const USER_CAP_UTILIZATION: &str = "djinn_user_cap_utilization";
const SLOT_POOL: &str = "djinn_slot_pool";
const DISPATCH_OUTCOMES: [&str; 5] = ["ok", "cooldown", "cap", "breaker", "error"];
const BREAKER_TRIPS_TOTAL: &str = "djinn_breaker_trips_total";
const BREAKER_STATE: &str = "djinn_breaker_state";
const ZOMBIE_REAPS_TOTAL: &str = "djinn_zombie_reaps_total";
const ZOMBIE_REAP_KINDS: [&str; 3] = ["startup", "periodic", "stall"];
const LEAD_ESCALATIONS_TOTAL: &str = "djinn_lead_escalations_total";
const TASK_REOPENS_TOTAL: &str = "djinn_task_reopens_total";
const TASKS_PARKED_TOTAL: &str = "djinn_tasks_parked_total";
const PR_POLLER_TRACKED: &str = "djinn_pr_poller_tracked";
const MERGE_FAILURES_TOTAL: &str = "djinn_merge_failures_total";
const DISPATCH_COOLDOWNS_ACTIVE: &str = "djinn_dispatch_cooldowns_active";
const DISPATCH_LAST_SUCCESS_TIMESTAMP: &str = "djinn_dispatch_last_success_timestamp";
const SLOT_POOL: &str = "djinn_slot_pool";
const SLOT_POOL_STATES: [&str; 2] = ["free", "busy"];
const INFLIGHT_LEDGER_SIZE: &str = "djinn_inflight_ledger_size";
const USER_CAP_UTILIZATION: &str = "djinn_user_cap_utilization";

static HANDLE: OnceLock<Result<PrometheusHandle, String>> = OnceLock::new();

/// Install the process-global Prometheus recorder.
///
/// Initialization is synchronous and idempotent. The first caller installs the
/// global `metrics` recorder; later callers observe the same result without
/// taking application locks or requiring an async runtime.
pub fn init() -> Result<(), String> {
    handle().map(|_| ())
}

pub mod breaker {
    /// Increment the breaker-trip counter. Synchronous and non-async by design.
    pub fn increment_trip() {
        metrics::counter!(super::BREAKER_TRIPS_TOTAL).increment(1);
    }

    /// Set the scrape-time breaker-state gauge for a `(scope, model)` bucket.
    pub fn set_state(scope: &str, model: &str, value: f64) {
        metrics::gauge!(super::BREAKER_STATE, "scope" => scope.to_owned(), "model" => model.to_owned()).set(value);
    }
}

pub mod zombie {
    pub const KIND_STARTUP: &str = "startup";
    pub const KIND_PERIODIC: &str = "periodic";
    pub const KIND_STALL: &str = "stall";

    /// Increment the zombie-reap counter for one of the stable kind labels.
    pub fn increment_reap(kind: &'static str) {
        metrics::counter!(super::ZOMBIE_REAPS_TOTAL, "kind" => kind).increment(1);
    }
}

pub mod lead {
    /// Increment the Lead-escalation counter. Synchronous and non-async by design.
    pub fn increment_escalation() {
        metrics::counter!(super::LEAD_ESCALATIONS_TOTAL).increment(1);
    }
}

pub mod task {
    /// Increment the task-reopen counter when a transition successfully bumps `reopen_count`.
    pub fn increment_reopen() {
        metrics::counter!(super::TASK_REOPENS_TOTAL).increment(1);
    }

    /// Increment the parked-task counter when the coordinator records a terminal task park.
    pub fn increment_parked() {
        metrics::counter!(super::TASKS_PARKED_TOTAL).increment(1);
    }
}

pub mod pr_poller {
    /// Set the O(1)-cardinality tracked fast-path PR count.
    pub fn set_tracked(count: usize) {
        metrics::gauge!(super::PR_POLLER_TRACKED).set(count as f64);
    }

    /// Increment merge failures that fall through to PR-poller reopen handling.
    pub fn increment_merge_failure() {
        metrics::counter!(super::MERGE_FAILURES_TOTAL).increment(1);
    }
}

/// Render the current registry in Prometheus text format.
///
/// Calling this before `init()` is supported: it initializes the recorder first
/// so tests can exercise the render path directly.
pub fn render() -> Result<String, String> {
    handle().map(|handle| handle.render())
}

fn handle() -> Result<&'static PrometheusHandle, String> {
    HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .map_err(format_build_error)
                .inspect(|_| register_metrics())
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn format_build_error(error: BuildError) -> String {
    format!("failed to install Prometheus metrics recorder: {error}")
}

fn register_metrics() {
    metrics::describe_counter!(
        DISPATCH_ATTEMPTS_TOTAL,
        "Dispatch attempts partitioned by terminal dispatch outcome."
    );
    for outcome in DISPATCH_OUTCOMES {
        metrics::counter!(DISPATCH_ATTEMPTS_TOTAL, "outcome" => outcome).absolute(0);
    }
    metrics::describe_gauge!(
        DISPATCH_LAST_SUCCESS_TIMESTAMP,
        "Unix timestamp in seconds for the last successful dispatch."
    );
    metrics::describe_gauge!(
        DISPATCH_COOLDOWNS_ACTIVE,
        "Current number of active dispatch cooldown entries."
    );
    metrics::describe_gauge!(
        INFLIGHT_LEDGER_SIZE,
        "Current number of entries in the coordinator in-flight dispatch ledger."
    );
    metrics::describe_gauge!(
        USER_CAP_UTILIZATION,
        "Per user/model dispatch cap utilization ratio: (db_running plus in-flight ledger overlay) divided by configured cap."
    );
    metrics::describe_gauge!(
        SLOT_POOL,
        "Slot pool slots aggregated by state and model. Labels are state=free|busy and model only."
    );
    metrics::describe_counter!(
        BREAKER_TRIPS_TOTAL,
        "Circuit-breaker trips at the authoritative closed-to-open transition."
    );
    metrics::counter!(BREAKER_TRIPS_TOTAL).absolute(0);
    metrics::describe_gauge!(
        BREAKER_STATE,
        "Circuit-breaker state by scope and model: Closed=0.0, HalfOpen=0.5, Open=1.0."
    );
    metrics::describe_counter!(
        ZOMBIE_REAPS_TOTAL,
        "Zombie reaps partitioned by reaper kind."
    );
    for kind in ZOMBIE_REAP_KINDS {
        metrics::counter!(ZOMBIE_REAPS_TOTAL, "kind" => kind).absolute(0);
    }
    metrics::describe_counter!(
        LEAD_ESCALATIONS_TOTAL,
        "Lead escalation requests recorded by the coordinator."
    );
    metrics::counter!(LEAD_ESCALATIONS_TOTAL).absolute(0);
    metrics::describe_counter!(TASK_REOPENS_TOTAL, "Tasks reopened for another work cycle.");
    metrics::counter!(TASK_REOPENS_TOTAL).absolute(0);
    metrics::describe_counter!(
        TASKS_PARKED_TOTAL,
        "Tasks terminally parked by coordinator safeguards."
    );
    metrics::counter!(TASKS_PARKED_TOTAL).absolute(0);
    metrics::describe_gauge!(
        PR_POLLER_TRACKED,
        "Number of PR-poller clean-merge fast-path tasks currently tracked."
    );
    metrics::gauge!(PR_POLLER_TRACKED).set(0.0);
    metrics::describe_counter!(
        MERGE_FAILURES_TOTAL,
        "PR merge failures that fall back to task reopen/rework."
    );
    metrics::counter!(MERGE_FAILURES_TOTAL).absolute(0);
    metrics::describe_gauge!(
        DISPATCH_COOLDOWNS_ACTIVE,
        "Active dispatch cooldown entries."
    );
    metrics::gauge!(DISPATCH_COOLDOWNS_ACTIVE).set(0.0);
    metrics::describe_gauge!(
        DISPATCH_LAST_SUCCESS_TIMESTAMP,
        "Unix timestamp of the last successful dispatch."
    );
    metrics::gauge!(DISPATCH_LAST_SUCCESS_TIMESTAMP).set(0.0);
    metrics::describe_gauge!(SLOT_POOL, "Slot-pool slots by state and model.");
    for state in SLOT_POOL_STATES {
        metrics::gauge!(SLOT_POOL, "state" => state, "model" => "").set(0.0);
    }
    metrics::describe_gauge!(
        INFLIGHT_LEDGER_SIZE,
        "Number of coordinator in-flight dispatch ledger entries."
    );
    metrics::gauge!(INFLIGHT_LEDGER_SIZE).set(0.0);
    metrics::describe_gauge!(
        USER_CAP_UTILIZATION,
        "Per-user/per-model running utilization against dispatch caps."
    );
    metrics::gauge!(USER_CAP_UTILIZATION, "user" => "", "model" => "").set(0.0);
}

pub mod dispatch {
    pub const OUTCOME_OK: &str = "ok";
    pub const OUTCOME_COOLDOWN: &str = "cooldown";
    pub const OUTCOME_CAP: &str = "cap";
    pub const OUTCOME_BREAKER: &str = "breaker";
    pub const OUTCOME_ERROR: &str = "error";

    /// Increment the dispatch-attempt counter for one of the stable outcome labels.
    ///
    /// This is intentionally synchronous and non-async so dispatch hot paths never
    /// need to hold any application lock across an await to emit telemetry.
    pub fn increment_attempt(outcome: &'static str) {
        metrics::counter!(super::DISPATCH_ATTEMPTS_TOTAL, "outcome" => outcome).increment(1);
    }

    pub fn record_last_success_now() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |duration| duration.as_secs_f64());
        metrics::gauge!(super::DISPATCH_LAST_SUCCESS_TIMESTAMP).set(ts);
    }

    pub fn set_cooldowns_active(count: usize) {
        metrics::gauge!(super::DISPATCH_COOLDOWNS_ACTIVE).set(count as f64);
    }

    pub fn set_inflight_ledger_size(count: usize) {
        metrics::gauge!(super::INFLIGHT_LEDGER_SIZE).set(count as f64);
    }

    /// Record a per-user/model cap-utilization ratio.
    ///
    /// `djinn_user_cap_utilization{user,model}` is a single gauge because the
    /// current metrics facade does not expose paired numerator/denominator
    /// samples. The convention is `used / cap`, where `used` is the same
    /// DB-running count overlaid with the coordinator in-flight ledger used for
    /// admission control, and `cap` is the configured per-user/model cap (default
    /// 1). Values may exceed 1.0 if live state was already over cap.
    pub fn set_user_cap_utilization(user: &str, model: &str, used: u32, cap: u32) {
        let cap = cap.max(1);
        let utilization = f64::from(used) / f64::from(cap);
        metrics::gauge!(super::USER_CAP_UTILIZATION, "user" => user.to_owned(), "model" => model.to_owned()).set(utilization);
    }

    pub fn record_success() {
        increment_attempt(OUTCOME_OK);
        record_last_success_now();
    }

    pub fn increment_ok() {
        increment_attempt(OUTCOME_OK);
    }

    pub fn increment_cooldown() {
        increment_attempt(OUTCOME_COOLDOWN);
    }

    pub fn increment_cap() {
        increment_attempt(OUTCOME_CAP);
    }

    pub fn increment_breaker() {
        increment_attempt(OUTCOME_BREAKER);
    }

    pub fn increment_error() {
        increment_attempt(OUTCOME_ERROR);
    }
}

pub mod slot_pool {
    pub const STATE_FREE: &str = "free";
    pub const STATE_BUSY: &str = "busy";

    pub fn set_slots(state: &'static str, model: &str, count: usize) {
        metrics::gauge!(super::SLOT_POOL, "state" => state, "model" => model.to_owned())
            .set(count as f64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static TEST_MUTEX: Mutex<()> = Mutex::new(());

    fn test_guard() -> MutexGuard<'static, ()> {
        TEST_MUTEX.lock().expect("telemetry test mutex poisoned")
    }

    fn rendered_sample<'a>(rendered: &'a str, metric: &str, labels: &[(&str, &str)]) -> &'a str {
        rendered
            .lines()
            .find(|line| {
                line.starts_with(metric)
                    && labels
                        .iter()
                        .all(|(key, value)| line.contains(&format!("{key}=\"{value}\"")))
            })
            .unwrap_or_else(|| panic!("missing sample {metric}{labels:?} in:\n{rendered}"))
    }

    #[test]
    fn init_is_idempotent_and_registers_dispatch_labels() {
        let _guard = test_guard();
        init().unwrap();
        init().unwrap();

        let rendered = render().unwrap();
        for outcome in DISPATCH_OUTCOMES {
            assert!(
                rendered.contains(&format!(
                    "djinn_dispatch_attempts_total{{outcome=\"{outcome}\"}}"
                )),
                "missing dispatch outcome label {outcome} in:\n{rendered}"
            );
        }
    }

    #[test]
    fn dispatch_attempt_increment_renders_counter() {
        let _guard = test_guard();
        init().unwrap();
        dispatch::increment_ok();

        let rendered = render().unwrap();
        assert!(rendered.contains("djinn_dispatch_attempts_total{outcome=\"ok\"}"));
    }

    #[test]
    fn live_state_gauges_render_with_bounded_labels() {
        let _guard = test_guard();
        init().unwrap();
        dispatch::set_cooldowns_active(2);
        dispatch::set_inflight_ledger_size(3);
        dispatch::set_user_cap_utilization("user-a", "model-a", 1, 2);
        slot_pool::set_slots(slot_pool::STATE_FREE, "model-a", 4);
        slot_pool::set_slots(slot_pool::STATE_BUSY, "model-a", 5);

        let rendered = render().unwrap();
        assert!(rendered.contains("djinn_dispatch_cooldowns_active 2"));
        assert!(rendered.contains("djinn_inflight_ledger_size 3"));
        assert!(
            rendered_sample(
                &rendered,
                "djinn_user_cap_utilization",
                &[("user", "user-a"), ("model", "model-a")]
            )
            .ends_with(" 0.5"),
            "user-cap gauge must use the documented used/cap ratio convention:\n{rendered}"
        );
        assert!(
            rendered_sample(
                &rendered,
                "djinn_slot_pool",
                &[("state", "free"), ("model", "model-a")]
            )
            .ends_with(" 4")
        );
        assert!(
            rendered_sample(
                &rendered,
                "djinn_slot_pool",
                &[("state", "busy"), ("model", "model-a")]
            )
            .ends_with(" 5")
        );
        assert!(!rendered.contains("slot_id="));
    }

    #[test]
    fn metric_facade_helpers_are_synchronous_unit_functions() {
        let _guard = test_guard();

        fn assert_sync_unit<F: FnOnce() -> ()>(f: F) {
            f();
        }

        init().unwrap();
        assert_sync_unit(|| dispatch::increment_attempt(dispatch::OUTCOME_ERROR));
        assert_sync_unit(|| dispatch::record_last_success_now());
        assert_sync_unit(|| dispatch::set_cooldowns_active(0));
        assert_sync_unit(|| dispatch::set_inflight_ledger_size(0));
        assert_sync_unit(|| dispatch::set_user_cap_utilization("user-sync", "model-sync", 0, 1));
        assert_sync_unit(|| slot_pool::set_slots(slot_pool::STATE_FREE, "model-sync", 0));
    }

    #[test]
    fn breaker_state_gauge_renders_scope_and_model() {
        let _guard = test_guard();
        init().unwrap();

        breaker::set_state("user-1", "model-a", 0.5);

        let rendered = render().unwrap();
        assert!(rendered.contains("djinn_breaker_state"));
        assert!(rendered.contains("model-a"));
        assert!(rendered.contains("user-1"));
        assert!(rendered.contains(" 0.5"));
    }

    #[test]
    fn zombie_and_lead_counters_render() {
        let _guard = test_guard();
        init().unwrap();

        zombie::increment_reap(zombie::KIND_STARTUP);
        zombie::increment_reap(zombie::KIND_PERIODIC);
        zombie::increment_reap(zombie::KIND_STALL);
        lead::increment_escalation();

        let rendered = render().unwrap();
        for kind in ZOMBIE_REAP_KINDS {
            assert!(rendered.contains(&format!("djinn_zombie_reaps_total{{kind=\"{kind}\"}}")));
        }
        assert!(rendered.contains("djinn_lead_escalations_total"));
    }

    #[test]
    fn task_and_pr_poller_metrics_render() {
        init().unwrap();

        task::increment_reopen();
        task::increment_parked();
        pr_poller::set_tracked(2);
        pr_poller::increment_merge_failure();

        let rendered = render().unwrap();
        assert!(rendered.contains("djinn_task_reopens_total"));
        assert!(rendered.contains("djinn_tasks_parked_total"));
        assert!(rendered.contains("djinn_pr_poller_tracked"));
        assert!(rendered.contains("djinn_merge_failures_total"));
    }
}

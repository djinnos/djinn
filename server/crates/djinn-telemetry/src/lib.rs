use std::sync::OnceLock;

use metrics_exporter_prometheus::{BuildError, PrometheusBuilder, PrometheusHandle};

pub const PROMETHEUS_TEXT_CONTENT_TYPE: &str = "text/plain; version=0.0.4";

const DISPATCH_ATTEMPTS_TOTAL: &str = "djinn_dispatch_attempts_total";
const DISPATCH_OUTCOMES: [&str; 5] = ["ok", "cooldown", "cap", "breaker", "error"];
const BREAKER_TRIPS_TOTAL: &str = "djinn_breaker_trips_total";
const BREAKER_STATE: &str = "djinn_breaker_state";
const ZOMBIE_REAPS_TOTAL: &str = "djinn_zombie_reaps_total";
const ZOMBIE_REAP_KINDS: [&str; 3] = ["startup", "periodic", "stall"];
const LEAD_ESCALATIONS_TOTAL: &str = "djinn_lead_escalations_total";

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent_and_registers_dispatch_labels() {
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
        init().unwrap();
        dispatch::increment_ok();

        let rendered = render().unwrap();
        assert!(rendered.contains("djinn_dispatch_attempts_total{outcome=\"ok\"}"));
    }

    #[test]
    fn breaker_state_gauge_renders_scope_and_model() {
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
}

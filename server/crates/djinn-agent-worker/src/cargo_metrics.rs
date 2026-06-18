//! Thin metric wrapper module for Cargo target seed and warm-base telemetry.
//!
//! Each function emits both a structured `tracing::info!` log line and a
//! Prometheus counter/gauge via `djinn_telemetry::cargo_cache`. Keeping the
//! wrapper in the worker crate keeps the call sites local and lets us swap the
//! underlying telemetry implementation without touching `main.rs`.

use tracing::info;

/// Log + metric for a successful warm-base seed.
pub fn record_seed_hit(project_id: &str) {
    info!(
        project_id,
        metric = "djinn_cargo_seed_hit_total",
        "cargo_metrics: seed hit"
    );
    djinn_telemetry::cargo_cache::record_seed_hit(project_id);
}

/// Log + metric for a cold-start fallback.
pub fn record_seed_cold(project_id: &str, reason: &str) {
    info!(
        project_id,
        fallback_reason = reason,
        metric = "djinn_cargo_seed_cold_total",
        "cargo_metrics: seed cold fallback"
    );
    djinn_telemetry::cargo_cache::record_seed_cold(project_id, reason);
}

/// Log + metric for warm-base freshness timing.
pub fn record_warm_base_freshness(project_id: &str, elapsed_ms: u64) {
    let age_secs = elapsed_ms as f64 / 1000.0;
    info!(
        project_id,
        elapsed_ms,
        metric = "djinn_cargo_warm_base_freshness_seconds",
        "cargo_metrics: warm base freshness"
    );
    djinn_telemetry::cargo_cache::record_warm_base_freshness(project_id, age_secs);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static TEST_MUTEX: Mutex<()> = Mutex::new(());

    fn test_guard() -> MutexGuard<'static, ()> {
        TEST_MUTEX
            .lock()
            .expect("cargo_metrics test mutex poisoned")
    }

    #[test]
    fn record_seed_hit_logs_with_project_id() {
        let _guard = test_guard();
        // Should not panic and should emit a structured log line
        record_seed_hit("project-hit-test");
    }

    #[test]
    fn record_seed_cold_logs_with_project_id_and_reason() {
        let _guard = test_guard();
        record_seed_cold("project-cold-test", "base_missing");
    }

    #[test]
    fn record_warm_base_freshness_logs_with_project_id_and_elapsed_ms() {
        let _guard = test_guard();
        record_warm_base_freshness("project-freshness-test", 2500);
    }
}

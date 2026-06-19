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

/// Log + metric for a single cargo warm-step invocation. `label` is the
/// call-site label string (e.g. `"clippy"`, `"build (clippy fallback)"`,
/// `"test --no-run"`); the function maps it to a bounded `step` metric label
/// so cardinality stays bounded. `outcome` is one of the stable
/// `djinn_telemetry::cargo_warm_step::OUTCOME_*` constants.
pub fn record_warm_step(project_id: &str, label: &str, outcome: &'static str) {
    let step = warm_step_metric_label(label);
    info!(
        project_id,
        step = step,
        outcome,
        metric = "djinn_cargo_warm_step_total",
        "cargo_metrics: warm step"
    );
    djinn_telemetry::cargo_warm_step::increment_step(project_id, step, outcome);
}

/// Log + metric for the resolved absolute cargo workspace directory.
/// The path is stored as a low-cardinality FNV-1a hash gauge so the
/// coordinator health sweep can correlate it with structured tracing
/// events without exploding label cardinality.
pub fn record_resolved_workspace_dir(project_id: &str, workspace_dir: &str) {
    info!(
        project_id,
        workspace_dir,
        metric = "djinn_cargo_warm_step_workspace_path_hash",
        "cargo_metrics: resolved cargo workspace dir"
    );
    djinn_telemetry::cargo_warm_step::set_workspace_path(project_id, workspace_dir);
}

/// Map the warm-step call-site label string to the bounded `STEP_*` constant
/// the metric uses. Unknown labels are coerced to `STEP_BUILD_FALLBACK` so
/// the metric label cardinality stays bounded.
fn warm_step_metric_label(label: &str) -> &'static str {
    match label {
        "clippy" => djinn_telemetry::cargo_warm_step::STEP_CLIPPY,
        "build (clippy fallback)" => djinn_telemetry::cargo_warm_step::STEP_BUILD_FALLBACK,
        "test --no-run" => djinn_telemetry::cargo_warm_step::STEP_TEST_NO_RUN,
        _ => djinn_telemetry::cargo_warm_step::STEP_BUILD_FALLBACK,
    }
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

    #[test]
    fn record_warm_step_maps_call_site_labels_to_bounded_step_constants() {
        let _guard = test_guard();

        // Known labels map to their STEP_* constant.
        assert_eq!(
            warm_step_metric_label("clippy"),
            djinn_telemetry::cargo_warm_step::STEP_CLIPPY,
        );
        assert_eq!(
            warm_step_metric_label("build (clippy fallback)"),
            djinn_telemetry::cargo_warm_step::STEP_BUILD_FALLBACK,
        );
        assert_eq!(
            warm_step_metric_label("test --no-run"),
            djinn_telemetry::cargo_warm_step::STEP_TEST_NO_RUN,
        );

        // Unknown labels collapse to STEP_BUILD_FALLBACK so cardinality
        // cannot leak through free-form call sites.
        assert_eq!(
            warm_step_metric_label("something unexpected"),
            djinn_telemetry::cargo_warm_step::STEP_BUILD_FALLBACK,
        );
        assert_eq!(
            warm_step_metric_label(""),
            djinn_telemetry::cargo_warm_step::STEP_BUILD_FALLBACK,
        );
    }

    #[test]
    fn record_warm_step_does_not_panic_for_known_and_unknown_labels() {
        let _guard = test_guard();
        record_warm_step(
            "project-warm-step-clippy",
            "clippy",
            djinn_telemetry::cargo_warm_step::OUTCOME_OK,
        );
        record_warm_step(
            "project-warm-step-test",
            "test --no-run",
            djinn_telemetry::cargo_warm_step::OUTCOME_FAILED,
        );
        record_warm_step(
            "project-warm-step-unknown",
            "novel step label",
            djinn_telemetry::cargo_warm_step::OUTCOME_SPAWN_ERROR,
        );
    }

    #[test]
    fn record_resolved_workspace_dir_logs_with_project_id() {
        let _guard = test_guard();
        record_resolved_workspace_dir(
            "project-warm-workspace-dir",
            "/workspace/proj-warm-workspace-dir/server",
        );
    }
}

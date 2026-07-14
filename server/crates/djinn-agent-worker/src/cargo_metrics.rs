//! Thin metric wrapper module for Cargo target seed and warm-base telemetry.
//!
//! Each function emits both a structured `tracing::info!` log line and a
//! Prometheus counter/gauge via `djinn_telemetry::cargo_cache`. Keeping the
//! wrapper in the worker crate keeps the call sites local and lets us swap the
//! underlying telemetry implementation without touching `main.rs`.

use tracing::info;

/// Closed classification for incremental-prune failures. These values are
/// structured-event data only; none are Prometheus labels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WarmIncrementalPruneErrorKind {
    Scan,
    Permission,
    Remove,
    TargetMismatch,
    InvalidProjectId,
    Symlink,
    LockOpen,
    LockProbe,
    LockAcquire,
}

impl WarmIncrementalPruneErrorKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Permission => "permission",
            Self::Remove => "remove",
            Self::TargetMismatch => "target_mismatch",
            Self::InvalidProjectId => "invalid_project_id",
            Self::Symlink => "symlink",
            Self::LockOpen => "lock_open",
            Self::LockProbe => "lock_probe",
            Self::LockAcquire => "lock_acquire",
        }
    }
}

/// The complete returned result surface for a warm incremental-prune attempt.
/// Keeping the error kind in this enum prevents callers from supplying a
/// free-form error string to metrics or the structured event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WarmIncrementalPruneResult {
    Pruned {
        pruned_bytes: u64,
    },
    AlreadyAbsent,
    UnsafePath {
        error_kind: WarmIncrementalPruneErrorKind,
    },
    Failed {
        error_kind: WarmIncrementalPruneErrorKind,
    },
}

/// Emit the terminal event and metric deltas for one returned warm
/// incremental-prune result.
///
/// Every result increments exactly one attempt. Logical bytes are added only
/// after a successful prune; absent, unsafe, and failed results add zero.
pub fn record_warm_incremental_prune(project_id: &str, result: WarmIncrementalPruneResult) {
    use djinn_telemetry::cargo_warm_incremental_prune::{self, Outcome};

    let (outcome, pruned_bytes, error_kind) = match result {
        WarmIncrementalPruneResult::Pruned { pruned_bytes } => {
            (Outcome::Pruned, pruned_bytes, None)
        }
        WarmIncrementalPruneResult::AlreadyAbsent => (Outcome::AlreadyAbsent, 0, None),
        WarmIncrementalPruneResult::UnsafePath { error_kind } => {
            (Outcome::UnsafePath, 0, Some(error_kind))
        }
        WarmIncrementalPruneResult::Failed { error_kind } => (Outcome::Failed, 0, Some(error_kind)),
    };
    let metric_attempt = 1_u64;
    let metric_bytes = pruned_bytes;

    info!(
        project_id,
        outcome = outcome.as_label(),
        pruned_bytes,
        metric_attempt,
        metric_bytes,
        error_kind = error_kind.map(WarmIncrementalPruneErrorKind::as_str),
        "cargo_metrics: warm incremental prune"
    );
    cargo_warm_incremental_prune::increment_attempt(project_id, outcome);
    if matches!(outcome, Outcome::Pruned) {
        cargo_warm_incremental_prune::add_pruned_bytes(project_id, pruned_bytes);
    }
}

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
        "clippy (all-features)" => djinn_telemetry::cargo_warm_step::STEP_CLIPPY,
        "clippy (default-features)" => {
            djinn_telemetry::cargo_warm_step::STEP_CLIPPY_DEFAULT_FEATURES
        }
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

    fn metric_value(rendered: &str, metric: &str, labels: &[(&str, &str)]) -> f64 {
        rendered
            .lines()
            .find(|line| {
                line.starts_with(metric)
                    && labels
                        .iter()
                        .all(|(key, value)| line.contains(&format!("{key}=\"{value}\"")))
            })
            .unwrap_or_else(|| panic!("missing {metric}{labels:?} in:\n{rendered}"))
            .rsplit_once(' ')
            .and_then(|(_, value)| value.parse().ok())
            .expect("metric sample should end with a number")
    }

    #[test]
    fn warm_incremental_prune_records_exact_deltas_for_outcomes_and_error_kinds() {
        let _guard = test_guard();
        djinn_telemetry::init().unwrap();

        let project_id = "worker-warm-incremental-prune-metrics";
        record_warm_incremental_prune(
            project_id,
            WarmIncrementalPruneResult::Pruned { pruned_bytes: 4096 },
        );
        record_warm_incremental_prune(project_id, WarmIncrementalPruneResult::AlreadyAbsent);
        record_warm_incremental_prune(
            project_id,
            WarmIncrementalPruneResult::UnsafePath {
                error_kind: WarmIncrementalPruneErrorKind::TargetMismatch,
            },
        );

        // Exercise every closed error kind. Error strings cannot enter metric
        // labels because the wrapper accepts this enum rather than `&str`.
        for error_kind in [
            WarmIncrementalPruneErrorKind::Scan,
            WarmIncrementalPruneErrorKind::Permission,
            WarmIncrementalPruneErrorKind::Remove,
            WarmIncrementalPruneErrorKind::TargetMismatch,
            WarmIncrementalPruneErrorKind::InvalidProjectId,
            WarmIncrementalPruneErrorKind::Symlink,
            WarmIncrementalPruneErrorKind::LockOpen,
            WarmIncrementalPruneErrorKind::LockProbe,
            WarmIncrementalPruneErrorKind::LockAcquire,
        ] {
            record_warm_incremental_prune(
                project_id,
                WarmIncrementalPruneResult::Failed { error_kind },
            );
        }

        let rendered = djinn_telemetry::render().unwrap();
        let attempts = djinn_telemetry::cargo_warm_incremental_prune::TOTAL;
        for (outcome, expected) in [
            ("pruned", 1.0),
            ("already_absent", 1.0),
            ("unsafe_path", 1.0),
            ("failed", 9.0),
        ] {
            assert_eq!(
                metric_value(
                    &rendered,
                    attempts,
                    &[("project_id", project_id), ("outcome", outcome)],
                ),
                expected,
                "unexpected attempt delta for {outcome}",
            );
        }
        assert_eq!(
            metric_value(
                &rendered,
                djinn_telemetry::cargo_warm_incremental_prune::PRUNED_BYTES_TOTAL,
                &[("project_id", project_id)],
            ),
            4096.0,
            "absent, unsafe, and failed attempts must add zero logical bytes",
        );
        for line in rendered.lines() {
            if line.starts_with(attempts) {
                assert!(
                    !line.contains("error_kind="),
                    "incremental-prune errors must not be metric labels: {line}"
                );
            }
        }
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
            warm_step_metric_label("clippy (all-features)"),
            djinn_telemetry::cargo_warm_step::STEP_CLIPPY,
        );
        assert_eq!(
            warm_step_metric_label("clippy (default-features)"),
            djinn_telemetry::cargo_warm_step::STEP_CLIPPY_DEFAULT_FEATURES,
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

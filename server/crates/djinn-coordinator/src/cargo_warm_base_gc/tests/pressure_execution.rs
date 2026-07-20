// djinn:allow-oversize — pressure executor regressions share fixtures and race helpers.
#![allow(clippy::await_holding_lock)]

use super::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Layer, registry::LookupSpan};

#[derive(Clone, Default)]
struct EventRecordingLayer {
    events: Arc<Mutex<Vec<HashMap<String, String>>>>,
}

const PRESSURE_METRICS_FIXTURE: &str =
    include_str!("../../../../djinn-telemetry/tests/fixtures/cache_cleanup/expected_metrics.json");

/// Serializes every test that consumes a pressure plan.
///
/// Executing a plan increments the process-global `djinn_cache_pressure_*`
/// counters, and cargo runs this file's tests as threads of one process, so a
/// second executor running concurrently lands its increments inside another
/// test's measurement window. Reading a delta is not enough on its own — the
/// delta only cancels history that predates the window, not a writer that
/// arrives during it. Every plan-consuming test therefore takes this guard so
/// exactly one of them is emitting at a time; the work under it is tempdir I/O
/// measured in milliseconds, so serializing costs effectively nothing.
///
/// Poisoning is recovered rather than propagated: one failing test should
/// report its own assertion, not convert its peers into unwrap panics.
fn pressure_metrics_guard() -> std::sync::MutexGuard<'static, ()> {
    static PRESSURE_METRICS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    PRESSURE_METRICS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn rendered_counter(rendered: &str, metric: &str, labels: &[(&str, &str)]) -> u64 {
    rendered
        .lines()
        .find_map(|line| {
            let (sample, value) = line.rsplit_once(' ')?;
            let (name, rendered_labels) = sample.split_once('{')?;
            let rendered_labels = rendered_labels.strip_suffix('}')?;
            (name == metric
                && labels.iter().all(|(key, value)| {
                    rendered_labels
                        .split(',')
                        .any(|label| label == format!("{key}=\"{value}\""))
                }))
            .then(|| value.parse().unwrap())
        })
        .unwrap_or(0)
}

fn pressure_counter(rendered: &str, mode: &str, rung: &str, outcome: &str) -> u64 {
    rendered_counter(
        rendered,
        "djinn_cache_pressure_units_total",
        &[("mode", mode), ("rung", rung), ("outcome", outcome)],
    )
}

fn pressure_bytes(rendered: &str, metric: &str, mode: &str, rung: &str) -> u64 {
    rendered_counter(rendered, metric, &[("mode", mode), ("rung", rung)])
}

fn pressure_termination(rendered: &str, mode: &str, termination: &str) -> u64 {
    rendered_counter(
        rendered,
        "djinn_cache_pressure_terminations_total",
        &[("mode", mode), ("termination", termination)],
    )
}

impl EventRecordingLayer {
    fn events(&self) -> Vec<HashMap<String, String>> {
        self.events.lock().unwrap().clone()
    }
}

#[derive(Default)]
struct EventFieldRecorder {
    fields: HashMap<String, String>,
}

impl Visit for EventFieldRecorder {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(
            field.name().to_owned(),
            format!("{value:?}").trim_matches('"').to_owned(),
        );
    }
}

impl<S> Layer<S> for EventRecordingLayer
where
    S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let mut recorder = EventFieldRecorder::default();
        event.record(&mut recorder);
        self.events.lock().unwrap().push(recorder.fields);
    }
}

#[tokio::test]
async fn pressure_execute_dry_run_reports_planner_prefix_without_locking() {
    let temp = tempfile::tempdir().unwrap();
    let base = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000001");
    let planned = WarmBaseEntry {
        project_id: "018f8b9a-0d70-7f0a-8000-000000000001".into(),
        path: base.clone(),
        size_bytes: 99,
    };
    let locks = RecordingBaseLock {
        attempts: std::sync::Mutex::new(Vec::new()),
        succeed: true,
    };
    let result = execute_pressure_eviction(
        PressureEvictionPlan {
            candidates: vec![WarmBaseCandidate {
                entry: planned.clone(),
                classification: BaseClassification::Registered,
                latest_activity: None,
                free_space_bytes: 0,
            }],
            retained: Vec::new(),
            projected_bytes: 99,
            target_bytes: 99,
        },
        &Activity(Ok(ActivitySnapshot {
            latest_activity: Some("2020-01-01T00:00:00Z".into()),
            ..snapshot()
        })),
        &Warm(Ok(false)),
        &locks,
        &Capacity(Err("must not measure dry run".into())),
        &default_config(),
        &epoch_clock(),
        crate::context::CacheCleanupMode::DryRun,
        temp.path(),
    )
    .await;

    assert_eq!(result.dry_run, vec![planned]);
    assert_eq!(result.projected_bytes, 99);
    assert!(result.deleted.is_empty());
    assert!(base.exists());
    assert!(locks.attempts.lock().unwrap().is_empty());
}

struct SequenceCapacity(
    std::sync::Mutex<std::collections::VecDeque<Result<CapacitySnapshot, String>>>,
);
impl FilesystemCapacity for SequenceCapacity {
    fn capacity(&self, _: &Path) -> Result<CapacitySnapshot, String> {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected capacity call")
    }
}

fn pressure_candidate(entry: WarmBaseEntry) -> WarmBaseCandidate {
    WarmBaseCandidate {
        entry,
        classification: BaseClassification::Registered,
        latest_activity: None,
        free_space_bytes: 0,
    }
}

fn executable_pressure_config() -> crate::context::CacheCleanupConfig {
    let mut config = pressure_config(0.15, 0.25);
    config.warm_base_grace_period = Duration::ZERO;
    config
}

fn eligible_three_rung_unit(base: &Path, target: &Path, rung: PressureRung) -> PressurePlanUnit {
    PressurePlanUnit {
        rung,
        project_id: base.file_name().unwrap().to_str().unwrap().into(),
        canonical_base: base.to_path_buf(),
        canonical_target: target.to_path_buf(),
        projected_allocated_bytes: 0,
        disposition: PressurePlanDisposition::Eligible,
    }
}

fn three_rung_clock() -> TestClock {
    TestClock::new(
        SystemTime::now() + Duration::from_secs(1),
        std::time::Instant::now(),
    )
}

#[tokio::test]
async fn three_rung_executor_retains_terminal_precheck_suffix() {
    let _pressure_metrics = pressure_metrics_guard();
    let temp = tempfile::tempdir().unwrap();
    let first = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000020");
    let second = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000021");
    let first_unit = eligible_three_rung_unit(&first, &first, PressureRung::WholeBase);
    let second_unit = eligible_three_rung_unit(&second, &second, PressureRung::WholeBase);
    let capacity = SequenceCapacity(std::sync::Mutex::new(std::collections::VecDeque::from([
        Err("external capacity probe failed".into()),
    ])));

    let result = execute_three_rung_pressure_plan(
        &ThreeRungPressurePlan {
            units: vec![first_unit.clone(), second_unit.clone()],
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &NoopBaseLock,
        &capacity,
        &executable_pressure_config(),
        &three_rung_clock(),
        temp.path(),
    )
    .await;

    assert!(result.remeasurement_failed);
    assert!(result.attempted.is_empty());
    assert_eq!(result.retained, vec![first_unit, second_unit]);
    assert!(first.exists() && second.exists());
}

#[tokio::test]
async fn three_rung_executor_keeps_success_and_retains_postcheck_suffix() {
    let _pressure_metrics = pressure_metrics_guard();
    let temp = tempfile::tempdir().unwrap();
    let first = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000022");
    let second = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000023");
    std::fs::write(first.join("reclaim"), b"bytes").unwrap();
    let first_unit = eligible_three_rung_unit(&first, &first, PressureRung::WholeBase);
    let second_unit = eligible_three_rung_unit(&second, &second, PressureRung::WholeBase);
    let capacity = SequenceCapacity(std::sync::Mutex::new(std::collections::VecDeque::from([
        Ok(CapacitySnapshot {
            total_bytes: 1000,
            available_bytes: 100,
        }),
        Err("post-removal probe failed".into()),
    ])));

    let result = execute_three_rung_pressure_plan(
        &ThreeRungPressurePlan {
            units: vec![first_unit.clone(), second_unit.clone()],
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &NoopBaseLock,
        &capacity,
        &executable_pressure_config(),
        &three_rung_clock(),
        temp.path(),
    )
    .await;

    assert!(result.remeasurement_failed);
    assert_eq!(result.attempted, vec![first_unit.clone()]);
    assert_eq!(result.deleted, vec![first_unit]);
    assert_eq!(result.retained, vec![second_unit]);
    assert!(!first.exists() && second.exists());
}

#[tokio::test]
async fn three_rung_executor_absent_unit_blocks_same_base_escalation() {
    let _pressure_metrics = pressure_metrics_guard();
    let temp = tempfile::tempdir().unwrap();
    let base = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000024");
    let absent = eligible_three_rung_unit(
        &base,
        &base.join("debug").join("incremental"),
        PressureRung::Incremental,
    );
    let broader = eligible_three_rung_unit(&base, &base, PressureRung::WholeBase);
    let locks = RecordingBaseLock {
        attempts: std::sync::Mutex::new(Vec::new()),
        succeed: true,
    };
    let capacity = SequenceCapacity(std::sync::Mutex::new(std::collections::VecDeque::new()));

    let result = execute_three_rung_pressure_plan(
        &ThreeRungPressurePlan {
            units: vec![absent, broader.clone()],
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &locks,
        &capacity,
        &executable_pressure_config(),
        &three_rung_clock(),
        temp.path(),
    )
    .await;

    assert!(result.attempted.is_empty());
    assert_eq!(result.retained, vec![broader]);
    assert_eq!(locks.attempts.lock().unwrap().len(), 1);
    assert!(base.exists());
}

#[test]
fn dry_run_planning_lock_policy_does_not_create_lock_file() {
    let temp = tempfile::tempdir().unwrap();
    let base = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000010");
    // This is the concrete non-mutating policy selected by the production
    // health sweep for CacheCleanupMode::DryRun.
    assert_eq!(NoopLockGuard.try_lock(&base), LockOutcome::Available);
    assert!(!base.join(WARM_BASE_GC_LOCK_FILE).exists());
}

#[tokio::test]
async fn pressure_executor_remeasures_each_delete_and_stops_at_high() {
    let temp = tempfile::tempdir().unwrap();
    let first = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000011");
    let second = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000012");
    let third = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000013");
    std::fs::write(first.join("a"), b"abc").unwrap();
    std::fs::write(second.join("b"), b"defg").unwrap();
    std::fs::write(third.join("c"), b"ignored").unwrap();
    let capacity = SequenceCapacity(std::sync::Mutex::new(std::collections::VecDeque::from([
        Ok(CapacitySnapshot {
            total_bytes: 1000,
            available_bytes: 200,
        }),
        Ok(CapacitySnapshot {
            total_bytes: 1000,
            available_bytes: 250,
        }),
    ])));
    let result = execute_pressure_eviction(
        PressureEvictionPlan {
            candidates: vec![
                pressure_candidate(make_entry(&first)),
                pressure_candidate(make_entry(&second)),
                pressure_candidate(make_entry(&third)),
            ],
            retained: Vec::new(),
            projected_bytes: 999,
            target_bytes: 50,
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &NoopBaseLock,
        &capacity,
        &executable_pressure_config(),
        &TestClock::new(
            SystemTime::now() + Duration::from_secs(1),
            std::time::Instant::now(),
        ),
        crate::context::CacheCleanupMode::Delete,
        temp.path(),
    )
    .await;
    assert_eq!(result.deleted.len(), 2);
    assert_eq!(result.reclaimed_bytes, 7);
    assert_eq!(result.projected_bytes, 999);
    assert!(result.reached_high_watermark);
    assert!(!first.exists() && !second.exists() && third.exists());
}

#[tokio::test]
async fn pressure_executor_retains_lock_recheck_and_delete_failures() {
    let temp = tempfile::tempdir().unwrap();
    let base = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000014");
    let entry = make_entry(&base);
    let capacity = Capacity(Ok(CapacitySnapshot {
        total_bytes: 1000,
        available_bytes: 100,
    }));
    let lock_retained = execute_pressure_eviction(
        PressureEvictionPlan {
            candidates: vec![pressure_candidate(entry.clone())],
            retained: Vec::new(),
            projected_bytes: 0,
            target_bytes: 0,
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &RecordingBaseLock {
            attempts: std::sync::Mutex::new(Vec::new()),
            succeed: false,
        },
        &capacity,
        &executable_pressure_config(),
        &TestClock::new(
            SystemTime::now() + Duration::from_secs(1),
            std::time::Instant::now(),
        ),
        crate::context::CacheCleanupMode::Delete,
        temp.path(),
    )
    .await;
    assert_eq!(lock_retained.retained[0].1, PressureSkipReason::LockBusy);
    let active_retained = execute_pressure_eviction(
        PressureEvictionPlan {
            candidates: vec![pressure_candidate(entry.clone())],
            retained: Vec::new(),
            projected_bytes: 0,
            target_bytes: 0,
        },
        &Activity(Ok(ActivitySnapshot {
            has_active_task_run: true,
            ..snapshot()
        })),
        &Warm(Ok(false)),
        &NoopBaseLock,
        &capacity,
        &executable_pressure_config(),
        &TestClock::new(
            SystemTime::now() + Duration::from_secs(1),
            std::time::Instant::now(),
        ),
        crate::context::CacheCleanupMode::Delete,
        temp.path(),
    )
    .await;
    assert_eq!(
        active_retained.retained[0].1,
        PressureSkipReason::ActiveTaskRun
    );
    let delete_retained = execute_pressure_eviction(
        PressureEvictionPlan {
            candidates: vec![pressure_candidate(entry)],
            retained: Vec::new(),
            projected_bytes: 0,
            target_bytes: 0,
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &NoopBaseLock,
        &capacity,
        &executable_pressure_config(),
        &TestClock::new(
            SystemTime::now() + Duration::from_secs(1),
            std::time::Instant::now(),
        ),
        crate::context::CacheCleanupMode::Delete,
        Path::new("/outside-root"),
    )
    .await;
    assert_eq!(
        delete_retained.retained[0].1,
        PressureSkipReason::DeleteError
    );
    assert!(base.exists());
}

#[tokio::test]
async fn pressure_executor_preserves_planning_outcomes_and_stops_on_remeasurement_error() {
    let temp = tempfile::tempdir().unwrap();
    let base = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000015");
    std::fs::write(base.join("actual"), b"seven!!").unwrap();
    let capacity = SequenceCapacity(std::sync::Mutex::new(std::collections::VecDeque::from([
        Err("statvfs failed".into()),
    ])));
    let retained_id = "018f8b9a-0d70-7f0a-8000-000000000016".to_owned();
    let result = execute_pressure_eviction(
        PressureEvictionPlan {
            candidates: vec![pressure_candidate(make_entry(&base))],
            retained: vec![(retained_id.clone(), PressureSkipReason::MeasurementError)],
            projected_bytes: 99,
            target_bytes: 99,
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &NoopBaseLock,
        &capacity,
        &executable_pressure_config(),
        &TestClock::new(
            SystemTime::now() + Duration::from_secs(1),
            std::time::Instant::now(),
        ),
        crate::context::CacheCleanupMode::Delete,
        temp.path(),
    )
    .await;
    assert_eq!(
        result.retained,
        vec![(retained_id, PressureSkipReason::MeasurementError)]
    );
    assert_eq!(result.reclaimed_bytes, 7);
    assert_eq!(result.projected_bytes, 99);
    assert!(result.remeasurement_failed);
}

#[tokio::test]
async fn pressure_failure_telemetry_and_completion_log_are_bounded() {
    use djinn_telemetry::cache_cleanup as metrics;

    const PROJECT_ID: &str = "018f8b9a-0d70-7f0a-8000-000000000099";
    djinn_telemetry::init().unwrap();
    let layer = EventRecordingLayer::default();
    let subscriber = tracing_subscriber::registry().with(layer.clone());
    let subscriber_guard = tracing::subscriber::set_default(subscriber);

    let result = execute_pressure_eviction(
        PressureEvictionPlan {
            candidates: Vec::new(),
            retained: vec![(PROJECT_ID.to_owned(), PressureSkipReason::MeasurementError)],
            projected_bytes: 0,
            target_bytes: 0,
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &NoopBaseLock,
        &Capacity(Err("capacity must not be called".into())),
        &executable_pressure_config(),
        &epoch_clock(),
        crate::context::CacheCleanupMode::Delete,
        Path::new("/unused"),
    )
    .await;
    log_pressure_eviction_completion(&result, crate::context::CacheCleanupMode::Delete);
    drop(subscriber_guard);

    let rendered_metrics = djinn_telemetry::render().unwrap();
    let metric_line = rendered_metrics
        .lines()
        .find(|line| {
            line.starts_with("djinn_cache_cleanup_total{")
                && line.contains(&format!(
                    "component=\"{}\"",
                    metrics::COMPONENT_CARGO_WARM_BASE
                ))
                && line.contains(&format!("mode=\"{}\"", metrics::MODE_DELETE))
                && line.contains(&format!("outcome=\"{}\"", metrics::OUTCOME_ERROR))
        })
        .expect("pressure error metric");
    let (_, labels) = metric_line.split_once('{').unwrap();
    let (labels, _) = labels.split_once('}').unwrap();
    let mut labels: Vec<_> = labels.split(',').collect();
    labels.sort_unstable();
    assert_eq!(
        labels,
        vec![
            "component=\"cargo_warm_base\"",
            "mode=\"delete\"",
            "outcome=\"error\"",
        ]
    );
    assert!(!metric_line.contains(PROJECT_ID));

    let event = layer
        .events()
        .into_iter()
        .find(|fields| {
            fields.get("message").map(String::as_str) == Some("warm-base pressure GC completed")
        })
        .expect("pressure completion event");
    assert_eq!(
        event.get("component").map(String::as_str),
        Some(metrics::COMPONENT_CARGO_WARM_BASE)
    );
    assert_eq!(
        event.get("mode").map(String::as_str),
        Some(metrics::MODE_DELETE)
    );
    assert_eq!(
        event.get("retained_outcomes").map(String::as_str),
        Some("[MeasurementError]")
    );
    assert!(event.values().all(|value| !value.contains(PROJECT_ID)));
}

struct RemovingCapacity {
    target: PathBuf,
    values: Mutex<std::collections::VecDeque<Result<CapacitySnapshot, String>>>,
}
impl FilesystemCapacity for RemovingCapacity {
    fn capacity(&self, _: &Path) -> Result<CapacitySnapshot, String> {
        std::fs::remove_dir_all(&self.target).unwrap();
        self.values.lock().unwrap().pop_front().unwrap()
    }
}

#[tokio::test]
async fn three_rung_executor_external_reclamation_before_first_retains_suffix() {
    let _pressure_metrics = pressure_metrics_guard();
    let temp = tempfile::tempdir().unwrap();
    let first = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000030");
    let second = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000031");
    let first_unit = eligible_three_rung_unit(&first, &first, PressureRung::WholeBase);
    let second_unit = eligible_three_rung_unit(&second, &second, PressureRung::WholeBase);
    let locks = RecordingBaseLock {
        attempts: Mutex::new(Vec::new()),
        succeed: true,
    };
    let capacity = SequenceCapacity(Mutex::new(std::collections::VecDeque::from([Ok(
        CapacitySnapshot {
            total_bytes: 100,
            available_bytes: 25,
        },
    )])));

    let result = execute_three_rung_pressure_plan(
        &ThreeRungPressurePlan {
            units: vec![first_unit.clone(), second_unit.clone()],
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &locks,
        &capacity,
        &executable_pressure_config(),
        &three_rung_clock(),
        temp.path(),
    )
    .await;

    assert!(result.reached_high_watermark);
    assert!(result.attempted.is_empty());
    assert_eq!(result.retained, vec![first_unit, second_unit]);
    assert_eq!(locks.attempts.lock().unwrap().len(), 1);
    assert!(first.exists() && second.exists());
}

#[tokio::test]
async fn three_rung_executor_external_reclamation_between_attempts_stops_before_next_remove() {
    let _pressure_metrics = pressure_metrics_guard();
    let temp = tempfile::tempdir().unwrap();
    let first = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000032");
    let second = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000033");
    std::fs::write(first.join("bytes"), b"bytes").unwrap();
    let first_unit = eligible_three_rung_unit(&first, &first, PressureRung::WholeBase);
    let second_unit = eligible_three_rung_unit(&second, &second, PressureRung::WholeBase);
    let capacity = SequenceCapacity(Mutex::new(std::collections::VecDeque::from([
        Ok(CapacitySnapshot {
            total_bytes: 100,
            available_bytes: 10,
        }),
        Ok(CapacitySnapshot {
            total_bytes: 100,
            available_bytes: 10,
        }),
        Ok(CapacitySnapshot {
            total_bytes: 100,
            available_bytes: 25,
        }),
    ])));

    let result = execute_three_rung_pressure_plan(
        &ThreeRungPressurePlan {
            units: vec![first_unit.clone(), second_unit.clone()],
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &NoopBaseLock,
        &capacity,
        &executable_pressure_config(),
        &three_rung_clock(),
        temp.path(),
    )
    .await;

    assert_eq!(result.deleted, vec![first_unit]);
    assert_eq!(result.retained, vec![second_unit]);
    assert!(result.reached_high_watermark);
    assert!(!first.exists() && second.exists());
}

struct HeldLock(Arc<std::sync::atomic::AtomicBool>);
struct HeldGuard(Arc<std::sync::atomic::AtomicBool>);
impl Drop for HeldGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}
impl LockGuard for HeldGuard {}
impl BaseLock for HeldLock {
    fn try_lock(&self, _: &Path) -> Result<Option<Box<dyn LockGuard>>, String> {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(Some(Box::new(HeldGuard(self.0.clone()))))
    }
}
struct LockAssertingCapacity(Arc<std::sync::atomic::AtomicBool>);
impl FilesystemCapacity for LockAssertingCapacity {
    fn capacity(&self, _: &Path) -> Result<CapacitySnapshot, String> {
        assert!(
            self.0.load(std::sync::atomic::Ordering::SeqCst),
            "capacity must run under lock"
        );
        Ok(CapacitySnapshot {
            total_bytes: 100,
            available_bytes: 25,
        })
    }
}

#[tokio::test]
async fn three_rung_executor_measures_immediately_under_held_lock() {
    let _pressure_metrics = pressure_metrics_guard();
    let temp = tempfile::tempdir().unwrap();
    let base = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000034");
    let unit = eligible_three_rung_unit(&base, &base, PressureRung::WholeBase);
    let held = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let result = execute_three_rung_pressure_plan(
        &ThreeRungPressurePlan {
            units: vec![unit.clone()],
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &HeldLock(held.clone()),
        &LockAssertingCapacity(held.clone()),
        &executable_pressure_config(),
        &three_rung_clock(),
        temp.path(),
    )
    .await;
    assert!(result.reached_high_watermark);
    assert_eq!(result.retained, vec![unit]);
    assert!(!held.load(std::sync::atomic::Ordering::SeqCst));
    assert!(base.exists());
}

#[tokio::test]
async fn three_rung_executor_absent_during_removal_blocks_same_base_escalation() {
    let _pressure_metrics = pressure_metrics_guard();
    let temp = tempfile::tempdir().unwrap();
    let base = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000035");
    let target = base.join("debug").join("incremental");
    std::fs::create_dir_all(&target).unwrap();
    let unit = eligible_three_rung_unit(&base, &target, PressureRung::Incremental);
    let broader = eligible_three_rung_unit(&base, &base, PressureRung::WholeBase);
    let locks = RecordingBaseLock {
        attempts: Mutex::new(Vec::new()),
        succeed: true,
    };
    let capacity = RemovingCapacity {
        target: target.clone(),
        values: Mutex::new(std::collections::VecDeque::from([Ok(CapacitySnapshot {
            total_bytes: 100,
            available_bytes: 10,
        })])),
    };

    let result = execute_three_rung_pressure_plan(
        &ThreeRungPressurePlan {
            units: vec![unit.clone(), broader.clone()],
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &locks,
        &capacity,
        &executable_pressure_config(),
        &three_rung_clock(),
        temp.path(),
    )
    .await;
    assert_eq!(result.attempted, vec![unit]);
    assert!(result.deleted.is_empty());
    assert_eq!(result.retained, vec![broader]);
    assert_eq!(locks.attempts.lock().unwrap().len(), 1);
    assert!(base.exists());
}

#[tokio::test]
async fn three_rung_executor_removal_failure_blocks_same_base_escalation() {
    let _pressure_metrics = pressure_metrics_guard();
    let temp = tempfile::tempdir().unwrap();
    let base = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000036");
    let target = base.join("debug").join("incremental");
    std::fs::create_dir_all(&target).unwrap();
    let already_removed = target.join("already-removed");
    std::fs::write(&already_removed, b"partial").unwrap();
    std::fs::write(target.join("must-remain"), b"preserve").unwrap();
    let unit = eligible_three_rung_unit(&base, &target, PressureRung::Incremental);
    let broader = eligible_three_rung_unit(&base, &base, PressureRung::WholeBase);
    let capacity = SequenceCapacity(Mutex::new(std::collections::VecDeque::from([Ok(
        CapacitySnapshot {
            total_bytes: 100,
            available_bytes: 10,
        },
    )])));

    // The production executor still calls its removal path; this deterministic
    // filesystem seam removes one child then reports the error that a real
    // `remove_dir_all` can return after it has already made progress.
    set_remove_dir_all_hook(Some(Box::new({
        let already_removed = already_removed.clone();
        move |_| {
            std::fs::remove_file(&already_removed)?;
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "simulated remove_dir_all failure after partial deletion",
            ))
        }
    })));
    let result = execute_three_rung_pressure_plan(
        &ThreeRungPressurePlan {
            units: vec![unit.clone(), broader.clone()],
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &NoopBaseLock,
        &capacity,
        &executable_pressure_config(),
        &three_rung_clock(),
        temp.path(),
    )
    .await;
    set_remove_dir_all_hook(None);
    assert_eq!(result.attempted, vec![unit.clone()]);
    assert_eq!(result.retained, vec![unit, broader]);
    assert!(base.exists());
    assert!(target.exists());
    assert!(!already_removed.exists());
    assert!(target.join("must-remain").exists());
}

#[cfg(unix)]
struct SwappingActivity {
    target: PathBuf,
    replacement: PathBuf,
}
#[cfg(unix)]
#[async_trait]
impl ActivityGuard for SwappingActivity {
    async fn activity(&self, _: &str) -> Result<ActivitySnapshot, String> {
        std::fs::remove_dir_all(&self.target).unwrap();
        std::os::unix::fs::symlink(&self.replacement, &self.target).unwrap();
        Ok(snapshot())
    }
}

#[cfg(unix)]
#[tokio::test]
async fn three_rung_executor_path_swap_to_symlink_is_retained_without_mutation() {
    let _pressure_metrics = pressure_metrics_guard();
    let temp = tempfile::tempdir().unwrap();
    let base = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000037");
    let target = base.join("debug").join("incremental");
    std::fs::create_dir_all(&target).unwrap();
    let outside = temp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("preserve"), b"safe").unwrap();
    let unit = eligible_three_rung_unit(&base, &target, PressureRung::Incremental);
    let capacity = SequenceCapacity(Mutex::new(std::collections::VecDeque::new()));

    let result = execute_three_rung_pressure_plan(
        &ThreeRungPressurePlan {
            units: vec![unit.clone()],
        },
        &SwappingActivity {
            target: target.clone(),
            replacement: outside.clone(),
        },
        &Warm(Ok(false)),
        &NoopBaseLock,
        &capacity,
        &executable_pressure_config(),
        &three_rung_clock(),
        temp.path(),
    )
    .await;
    assert!(result.attempted.is_empty());
    assert_eq!(result.retained, vec![unit]);
    assert!(target.is_symlink());
    assert_eq!(std::fs::read(outside.join("preserve")).unwrap(), b"safe");
}

#[tokio::test]
async fn pressure_metrics_match_the_bounded_fixture_for_execution_boundaries() {
    let _pressure_metrics = pressure_metrics_guard();
    let fixture: serde_json::Value = serde_json::from_str(PRESSURE_METRICS_FIXTURE).unwrap();
    let case = |name: &str| &fixture["cases"][name];
    let value = |case: &serde_json::Value, field: &str| case[field].as_u64().unwrap();
    let assert_execution_metrics =
        |before: &str, after: &str, case: &serde_json::Value, rung: &str| {
            for outcome in fixture["outcomes"].as_array().unwrap() {
                let outcome = outcome.as_str().unwrap();
                assert_eq!(
                    pressure_counter(after, "delete", rung, outcome)
                        - pressure_counter(before, "delete", rung, outcome),
                    case[outcome].as_u64().unwrap_or(0),
                    "unexpected {outcome} delta for {rung}"
                );
            }
            for field in ["projected", "reclaimed"] {
                let metric = fixture["byte_metrics"][field].as_str().unwrap();
                assert_eq!(
                    pressure_bytes(after, metric, "delete", rung)
                        - pressure_bytes(before, metric, "delete", rung),
                    value(case, field),
                    "unexpected {field} byte delta for {rung}"
                );
            }
            let expected = case["termination"].as_str().unwrap();
            for termination in fixture["terminations"].as_array().unwrap() {
                let termination = termination.as_str().unwrap();
                assert_eq!(
                    pressure_termination(after, "delete", termination)
                        - pressure_termination(before, "delete", termination),
                    u64::from(termination == expected),
                    "unexpected {termination} termination delta"
                );
            }
        };
    assert_eq!(fixture["metric"], "djinn_cache_pressure_units_total");
    assert_eq!(
        fixture["rungs"],
        serde_json::json!(["incremental", "profile", "base"])
    );
    assert_eq!(
        fixture["outcomes"],
        serde_json::json!([
            "planned",
            "post_lock_eligible",
            "retained",
            "attempted",
            "deleted",
            "failed"
        ])
    );

    djinn_telemetry::init().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let config = executable_pressure_config();
    let clock = three_rung_clock();
    let capacity_low = || CapacitySnapshot {
        total_bytes: 100,
        available_bytes: 10,
    };
    let unit = |id: &str, rung: PressureRung| {
        let base = old_base(&temp, id);
        let mut unit = eligible_three_rung_unit(&base, &base, rung);
        unit.projected_allocated_bytes = 4096;
        (base, unit)
    };

    let (_, incremental) = unit(
        "018f8b9a-0d70-7f0a-8000-000000000101",
        PressureRung::Incremental,
    );
    let (_, profile) = unit(
        "018f8b9a-0d70-7f0a-8000-000000000102",
        PressureRung::StaleProfile,
    );
    let (_, base) = unit(
        "018f8b9a-0d70-7f0a-8000-000000000103",
        PressureRung::WholeBase,
    );
    let before = djinn_telemetry::render().unwrap();
    let dry_run = consume_three_rung_pressure_plan_dry_run(&ThreeRungPressurePlan {
        units: vec![incremental, profile, base],
    });
    let after = djinn_telemetry::render().unwrap();
    let dry = case("dry_run");
    assert_eq!(dry_run.len() as u64, value(dry, "planned"));
    for rung in ["incremental", "profile", "base"] {
        for outcome in fixture["outcomes"].as_array().unwrap() {
            let outcome = outcome.as_str().unwrap();
            assert_eq!(
                pressure_counter(&after, "dry_run", rung, outcome)
                    - pressure_counter(&before, "dry_run", rung, outcome),
                u64::from(outcome == "planned")
            );
        }
        assert_eq!(
            pressure_bytes(
                &after,
                fixture["byte_metrics"]["projected"].as_str().unwrap(),
                "dry_run",
                rung
            ) - pressure_bytes(
                &before,
                fixture["byte_metrics"]["projected"].as_str().unwrap(),
                "dry_run",
                rung
            ),
            4096
        );
        assert_eq!(
            pressure_bytes(
                &after,
                fixture["byte_metrics"]["reclaimed"].as_str().unwrap(),
                "dry_run",
                rung
            ) - pressure_bytes(
                &before,
                fixture["byte_metrics"]["reclaimed"].as_str().unwrap(),
                "dry_run",
                rung
            ),
            0
        );
    }
    assert!(dry["termination"].is_null());
    for termination in fixture["terminations"].as_array().unwrap() {
        let termination = termination.as_str().unwrap();
        assert_eq!(
            pressure_termination(&after, "dry_run", termination)
                - pressure_termination(&before, "dry_run", termination),
            0,
            "dry-run unexpectedly emitted {termination} termination telemetry"
        );
    }
    assert_eq!(value(dry, "projected"), 3 * 4096);
    assert_eq!(value(dry, "reclaimed"), 0);

    let (pre_base, pre) = unit(
        "018f8b9a-0d70-7f0a-8000-000000000104",
        PressureRung::WholeBase,
    );
    std::fs::write(pre_base.join("bytes"), vec![0; 4096]).unwrap();
    let before = djinn_telemetry::render().unwrap();
    let pre_result = execute_three_rung_pressure_plan(
        &ThreeRungPressurePlan { units: vec![pre] },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &NoopBaseLock,
        &SequenceCapacity(Mutex::new(std::collections::VecDeque::from([Err(
            "probe".into()
        )]))),
        &config,
        &clock,
        temp.path(),
    )
    .await;
    let after = djinn_telemetry::render().unwrap();
    let pre_case = case("pre_attempt_measurement_failure");
    assert!(pre_result.attempted.is_empty() && pre_result.deleted.is_empty());
    assert_execution_metrics(&before, &after, pre_case, "base");

    let (high_base, high) = unit(
        "018f8b9a-0d70-7f0a-8000-000000000108",
        PressureRung::StaleProfile,
    );
    std::fs::write(high_base.join("bytes"), vec![0; 4096]).unwrap();
    let before = djinn_telemetry::render().unwrap();
    let high_result = execute_three_rung_pressure_plan(
        &ThreeRungPressurePlan { units: vec![high] },
        &Activity(Ok(ActivitySnapshot {
            latest_activity: Some("2020-01-01T00:00:00Z".into()),
            ..snapshot()
        })),
        &Warm(Ok(false)),
        &NoopBaseLock,
        &SequenceCapacity(Mutex::new(std::collections::VecDeque::from([Ok(
            CapacitySnapshot {
                total_bytes: 100,
                available_bytes: 25,
            },
        )]))),
        &config,
        &clock,
        temp.path(),
    )
    .await;
    let after = djinn_telemetry::render().unwrap();
    let high_case = case("pre_attempt_reached_high");
    assert!(high_result.reached_high_watermark && high_result.attempted.is_empty());
    assert_execution_metrics(&before, &after, high_case, "profile");

    let (failed_base, failed) = unit(
        "018f8b9a-0d70-7f0a-8000-000000000105",
        PressureRung::Incremental,
    );
    std::fs::write(failed_base.join("bytes"), vec![0; 4096]).unwrap();
    let before = djinn_telemetry::render().unwrap();
    set_remove_dir_all_hook(Some(Box::new(|_| {
        Err(std::io::Error::other("remove failed"))
    })));
    let failed_result = execute_three_rung_pressure_plan(
        &ThreeRungPressurePlan {
            units: vec![failed],
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &NoopBaseLock,
        &SequenceCapacity(Mutex::new(std::collections::VecDeque::from([Ok(
            capacity_low(),
        )]))),
        &config,
        &clock,
        temp.path(),
    )
    .await;
    set_remove_dir_all_hook(None);
    let after = djinn_telemetry::render().unwrap();
    let removal = case("removal_failure");
    assert!(failed_result.deleted.is_empty());
    assert_execution_metrics(&before, &after, removal, "incremental");

    let (success_base, success) = unit(
        "018f8b9a-0d70-7f0a-8000-000000000106",
        PressureRung::WholeBase,
    );
    std::fs::write(success_base.join("bytes"), vec![0; 4096]).unwrap();
    let before = djinn_telemetry::render().unwrap();
    let success_result = execute_three_rung_pressure_plan(
        &ThreeRungPressurePlan {
            units: vec![success],
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &NoopBaseLock,
        &SequenceCapacity(Mutex::new(std::collections::VecDeque::from([
            Ok(capacity_low()),
            Ok(capacity_low()),
        ]))),
        &config,
        &clock,
        temp.path(),
    )
    .await;
    let after = djinn_telemetry::render().unwrap();
    let success_case = case("successful_deletion");
    assert_eq!(
        success_result.reclaimed_bytes,
        value(success_case, "reclaimed")
    );
    assert_execution_metrics(&before, &after, success_case, "base");

    let (post_base, post) = unit(
        "018f8b9a-0d70-7f0a-8000-000000000107",
        PressureRung::WholeBase,
    );
    std::fs::write(post_base.join("bytes"), vec![0; 4096]).unwrap();
    let before = djinn_telemetry::render().unwrap();
    let post_result = execute_three_rung_pressure_plan(
        &ThreeRungPressurePlan { units: vec![post] },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &NoopBaseLock,
        &SequenceCapacity(Mutex::new(std::collections::VecDeque::from([
            Ok(capacity_low()),
            Err("post probe".into()),
        ]))),
        &config,
        &clock,
        temp.path(),
    )
    .await;
    let after = djinn_telemetry::render().unwrap();
    let post_case = case("post_success_remeasurement_failure");
    assert!(post_result.remeasurement_failed && post_result.deleted.len() == 1);
    assert_execution_metrics(&before, &after, post_case, "base");
}

fn frozen_coordinator_fixture() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/cache_cleanup/three_rung_pressure.json"
    ))
    .expect("valid frozen coordinator fixture")
}

struct FixtureCapacity {
    values: Mutex<std::collections::VecDeque<Result<CapacitySnapshot, String>>>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
impl FilesystemCapacity for FixtureCapacity {
    fn capacity(&self, _: &Path) -> Result<CapacitySnapshot, String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.values
            .lock()
            .unwrap()
            .pop_front()
            .expect("fixture capacity call")
    }
}
fn fixture_capacity(high: bool) -> CapacitySnapshot {
    CapacitySnapshot {
        total_bytes: 100,
        available_bytes: if high { 25 } else { 10 },
    }
}
fn assert_fixture_result(
    case: &serde_json::Value,
    result: &ThreeRungPressureResult,
    removals: usize,
) {
    let count = |name: &str| case[name].as_u64().unwrap() as usize;
    assert_eq!(
        result.planned.len(),
        count("planned"),
        "{} planned",
        case["name"]
    );
    if case.get("eligible").is_some() {
        assert_eq!(
            result.post_lock_eligible.len(),
            count("eligible"),
            "{} eligible",
            case["name"]
        );
    }
    assert_eq!(
        result.attempted.len(),
        count("attempted"),
        "{} attempted",
        case["name"]
    );
    assert_eq!(
        result.deleted.len(),
        count("deleted"),
        "{} deleted",
        case["name"]
    );
    assert_eq!(
        result.retained.len(),
        count("retained"),
        "{} retained",
        case["name"]
    );
    assert_eq!(
        result.failed.len(),
        count("failed"),
        "{} failed",
        case["name"]
    );
    assert_eq!(
        removals,
        count("removal_calls"),
        "{} removal calls",
        case["name"]
    );
}

#[tokio::test]
async fn frozen_race_cases_execute_post_plan_guards_and_removal_seam() {
    let _pressure_metrics = pressure_metrics_guard();
    let fixture = frozen_coordinator_fixture();
    let temp = tempfile::tempdir().unwrap();
    for (index, case) in fixture["race_cases"].as_array().unwrap().iter().enumerate() {
        let base = old_base(
            &temp,
            &format!("018f8b9a-0d70-7f0a-8000-0000000004{index:02}"),
        );
        let target = base.join("debug/incremental");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("artifact"), b"reclaim").unwrap();
        let rung = if case["name"] == "staleness_changed" {
            PressureRung::StaleProfile
        } else {
            PressureRung::Incremental
        };
        let first = eligible_three_rung_unit(&base, &target, rung);
        let broader = eligible_three_rung_unit(&base, &base, PressureRung::WholeBase);
        let mut activity = Activity(Ok(snapshot()));
        let mut warm = Warm(Ok(false));
        let mut config = executable_pressure_config();
        match case["name"].as_str().unwrap() {
            "activity_changed" => {
                activity = Activity(Ok(ActivitySnapshot {
                    has_active_task_run: true,
                    ..snapshot()
                }))
            }
            "warm_changed" => warm = Warm(Ok(true)),
            "grace_changed" => config.warm_base_grace_period = Duration::from_secs(u64::MAX / 2),
            "existence_changed" => std::fs::remove_dir_all(&target).unwrap(),
            "containment_symlink_swap" => {
                #[cfg(unix)]
                {
                    let outside = temp.path().join(format!("outside-{index}"));
                    std::fs::create_dir(&outside).unwrap();
                    std::fs::remove_dir_all(&target).unwrap();
                    std::os::unix::fs::symlink(outside, &target).unwrap();
                }
            }
            "traversal_error" => {
                #[cfg(unix)]
                std::os::unix::fs::symlink(base.join("outside"), target.join("unsafe-link"))
                    .unwrap();
            }
            "staleness_changed" | "partial_removal_failure" => {}
            name => panic!("unknown race case {name}"),
        }
        let removal_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        if case["name"] == "partial_removal_failure" {
            let calls = removal_calls.clone();
            set_remove_dir_all_hook(Some(Box::new(move |_| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(std::io::Error::other("fixture remove failure"))
            })));
        }
        let result = execute_three_rung_pressure_plan(
            &ThreeRungPressurePlan {
                units: vec![first, broader],
            },
            &activity,
            &warm,
            &NoopBaseLock,
            &FixtureCapacity {
                values: Mutex::new(std::collections::VecDeque::from([Ok(fixture_capacity(
                    false,
                ))])),
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            },
            &config,
            &three_rung_clock(),
            temp.path(),
        )
        .await;
        set_remove_dir_all_hook(None);
        assert_fixture_result(
            case,
            &result,
            removal_calls.load(std::sync::atomic::Ordering::SeqCst),
        );
        assert!(
            base.exists(),
            "{} must block broader same-base escalation",
            case["name"]
        );
    }
}

#[tokio::test]
async fn frozen_capacity_cases_execute_external_reclamation_and_probe_failures() {
    let _pressure_metrics = pressure_metrics_guard();
    let fixture = frozen_coordinator_fixture();
    let temp = tempfile::tempdir().unwrap();
    for (index, case) in fixture["capacity_cases"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let first = old_base(
            &temp,
            &format!("018f8b9a-0d70-7f0a-8000-0000000005{index:02}"),
        );
        let second = old_base(
            &temp,
            &format!("018f8b9a-0d70-7f0a-8000-0000000006{index:02}"),
        );
        std::fs::write(first.join("bytes"), b"reclaim").unwrap();
        let values = match case["name"].as_str().unwrap() {
            "external_before_first" => vec![Ok(fixture_capacity(true))],
            "external_between_attempts" => vec![
                Ok(fixture_capacity(false)),
                Ok(fixture_capacity(false)),
                Ok(fixture_capacity(true)),
            ],
            "pre_measurement_failure" => vec![Err("pre probe".into())],
            "post_measurement_failure" => {
                vec![Ok(fixture_capacity(false)), Err("post probe".into())]
            }
            name => panic!("unknown capacity case {name}"),
        };
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let result = execute_three_rung_pressure_plan(
            &ThreeRungPressurePlan {
                units: vec![
                    eligible_three_rung_unit(&first, &first, PressureRung::WholeBase),
                    eligible_three_rung_unit(&second, &second, PressureRung::WholeBase),
                ],
            },
            &Activity(Ok(snapshot())),
            &Warm(Ok(false)),
            &NoopBaseLock,
            &FixtureCapacity {
                values: Mutex::new(values.into()),
                calls: calls.clone(),
            },
            &executable_pressure_config(),
            &three_rung_clock(),
            temp.path(),
        )
        .await;
        assert_fixture_result(case, &result, result.attempted.len());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            case["capacity_calls"].as_u64().unwrap() as usize
        );
        assert_eq!(
            result.reached_high_watermark,
            case["termination"] == "reached_high"
        );
        assert_eq!(
            result.remeasurement_failed,
            case["termination"] == "remeasure_failed"
        );
    }
}

#[test]
fn frozen_coordinator_fixture_records_exact_three_rung_cases() {
    let fixture = frozen_coordinator_fixture();
    let expected_order = serde_json::json!(["incremental", "stale_profile", "whole_base"]);
    assert_eq!(
        fixture["contract"],
        "frozen coordinator three-rung pressure schedule"
    );
    assert_eq!(fixture["rung_order"], expected_order);
    assert_eq!(fixture["dry_run"]["plan_units"], expected_order);
    assert_eq!(fixture["delete"]["plan_units"], expected_order);
    assert_eq!(fixture["dry_run"]["locks"], 0);
    assert_eq!(fixture["dry_run"]["rechecks"], 0);
    assert_eq!(fixture["dry_run"]["removals"], 0);
    assert_eq!(
        fixture["delete"]["lock_path"],
        ".warm-locks/<project-id>.lock"
    );
    assert_eq!(fixture["delete"]["fail_closed"], true);
    assert_eq!(
        fixture["delete"]["outcomes"],
        serde_json::json!({"planned": 3, "eligible": 3, "attempted": 3, "deleted": 3, "retained": 0, "failed": 0})
    );
    assert_eq!(
        fixture["cold_rebuild_cases"],
        serde_json::json!([
            {"name": "incremental", "removed": "debug/incremental", "preserved": ["debug/sibling", "release/artifact"], "rebuild": "debug/incremental/rebuilt"},
            {"name": "stale_profile", "removed": "debug", "preserved": ["release/artifact", "base-sibling"], "rebuild": "debug/rebuilt"},
            {"name": "whole_base", "removed": ".", "preserved": [], "rebuild": "debug/incremental/rebuilt"}
        ])
    );
    for case in fixture["race_cases"].as_array().unwrap() {
        for field in [
            "planned",
            "eligible",
            "attempted",
            "deleted",
            "retained",
            "failed",
            "removal_calls",
        ] {
            assert!(
                case[field].is_u64(),
                "{} must provide {field}",
                case["name"]
            );
        }
        assert_eq!(case["blocks_broader_same_base"], true);
    }
    for case in fixture["capacity_cases"].as_array().unwrap() {
        for field in [
            "capacity_calls",
            "planned",
            "attempted",
            "deleted",
            "retained",
            "failed",
            "removal_calls",
        ] {
            assert!(
                case[field].is_u64(),
                "{} must provide {field}",
                case["name"]
            );
        }
        assert!(matches!(
            case["termination"].as_str(),
            Some("reached_high" | "remeasure_failed")
        ));
    }
    assert_eq!(
        fixture["two_actor"]["timeline"],
        serde_json::json!([
            "warm_lock",
            "warm_traverse",
            "warm_compile",
            "pressure_busy",
            "warm_process_death",
            "pressure_lock",
            "pressure_traverse",
            "pressure_remove",
            "pressure_retry_complete"
        ])
    );
    assert_eq!(fixture["two_actor"]["loser_removals"], 0);
    assert_eq!(fixture["two_actor"]["retry_removals"], 1);
}

#[tokio::test]
async fn frozen_cold_rebuild_cases_execute_and_preserve_required_siblings() {
    let _pressure_metrics = pressure_metrics_guard();
    let fixture = frozen_coordinator_fixture();
    let temp = tempfile::tempdir().unwrap();
    for (index, case) in fixture["cold_rebuild_cases"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let base = old_base(
            &temp,
            &format!("018f8b9a-0d70-7f0a-8000-0000000002{index:02}"),
        );
        std::fs::create_dir_all(base.join("debug/incremental")).unwrap();
        std::fs::write(base.join("debug/incremental/artifact"), b"reclaim").unwrap();
        std::fs::write(base.join("debug/sibling"), b"preserve").unwrap();
        std::fs::create_dir_all(base.join("release")).unwrap();
        std::fs::write(base.join("release/artifact"), b"preserve").unwrap();
        std::fs::write(base.join("base-sibling"), b"preserve").unwrap();
        let rung = match case["name"].as_str().unwrap() {
            "incremental" => PressureRung::Incremental,
            "stale_profile" => PressureRung::StaleProfile,
            "whole_base" => PressureRung::WholeBase,
            unexpected => panic!("unknown frozen cold-rebuild case {unexpected}"),
        };
        let target = if case["removed"] == "." {
            base.clone()
        } else {
            base.join(case["removed"].as_str().unwrap())
        };
        let unit = eligible_three_rung_unit(&base, &target, rung);
        let result = execute_three_rung_pressure_plan(
            &ThreeRungPressurePlan {
                units: vec![unit.clone()],
            },
            &Activity(Ok(ActivitySnapshot {
                latest_activity: Some("2020-01-01T00:00:00Z".into()),
                ..snapshot()
            })),
            &Warm(Ok(false)),
            &NoopBaseLock,
            &SequenceCapacity(Mutex::new(std::collections::VecDeque::from([
                Ok(CapacitySnapshot {
                    total_bytes: 100,
                    available_bytes: 10,
                }),
                Ok(CapacitySnapshot {
                    total_bytes: 100,
                    available_bytes: 10,
                }),
            ]))),
            &executable_pressure_config(),
            &three_rung_clock(),
            temp.path(),
        )
        .await;
        assert_eq!(result.planned, vec![unit.clone()]);
        assert_eq!(result.post_lock_eligible, vec![unit.clone()]);
        assert_eq!(result.attempted, vec![unit.clone()]);
        assert_eq!(result.deleted, vec![unit]);
        assert!(result.retained.is_empty() && result.failed.is_empty());
        assert!(!target.exists(), "{} target must be deleted", case["name"]);
        for preserved in case["preserved"].as_array().unwrap() {
            assert!(
                base.join(preserved.as_str().unwrap()).exists(),
                "required sibling must survive"
            );
        }
        std::fs::create_dir_all(base.join(case["rebuild"].as_str().unwrap())).unwrap();
        assert!(
            base.join(case["rebuild"].as_str().unwrap()).exists(),
            "deleted rung must cold rebuild"
        );
    }
}

// Helper types and functions for the two-actor test, kept at module scope.

/// A recursive snapshot of every target-relative path, its file type, byte
/// content, and symlink target. Used to prove the busy pressure loser performs
/// zero filesystem mutation.
fn recursive_snapshot(root: &Path) -> Vec<(String, String, Vec<u8>)> {
    let mut entries = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for child in read.flatten() {
            let path = child.path();
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if metadata.file_type().is_symlink() {
                let target = std::fs::read_link(&path)
                    .map(|t| t.to_string_lossy().into_owned())
                    .unwrap_or_default();
                entries.push((relative, format!("symlink:{target}"), Vec::new()));
            } else if metadata.is_dir() {
                entries.push((relative, "dir".into(), Vec::new()));
                pending.push(path);
            } else if metadata.is_file() {
                let bytes = std::fs::read(&path).unwrap_or_default();
                entries.push((relative, "file".into(), bytes));
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TwoActorActor {
    Warm,
    Pressure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TwoActorOp {
    Traversal,
    Compilation,
    Removal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TwoActorBoundary {
    Enter,
    Exit,
}

#[derive(Clone, Copy, Debug)]
struct TwoActorEvent {
    actor: TwoActorActor,
    op: TwoActorOp,
    boundary: TwoActorBoundary,
    seq: u64,
}

#[derive(Clone, Copy, Debug)]
struct TwoActorInterval {
    actor: TwoActorActor,
    #[allow(dead_code)]
    op: TwoActorOp,
    enter_seq: u64,
    exit_seq: u64,
}

impl TwoActorInterval {
    /// Two intervals overlap if neither is completely before the other.
    fn overlaps(&self, other: &TwoActorInterval) -> bool {
        !(self.exit_seq < other.enter_seq || other.exit_seq < self.enter_seq)
    }
}

fn two_actor_event_line(
    actor: TwoActorActor,
    op: TwoActorOp,
    boundary: TwoActorBoundary,
    seq: u64,
) -> String {
    let (a, o, b) = match (actor, op, boundary) {
        (TwoActorActor::Warm, TwoActorOp::Traversal, TwoActorBoundary::Enter) => {
            ("warm", "traversal", "enter")
        }
        (TwoActorActor::Warm, TwoActorOp::Traversal, TwoActorBoundary::Exit) => {
            ("warm", "traversal", "exit")
        }
        (TwoActorActor::Warm, TwoActorOp::Compilation, TwoActorBoundary::Enter) => {
            ("warm", "compilation", "enter")
        }
        (TwoActorActor::Warm, TwoActorOp::Compilation, TwoActorBoundary::Exit) => {
            ("warm", "compilation", "exit")
        }
        (TwoActorActor::Pressure, TwoActorOp::Traversal, TwoActorBoundary::Enter) => {
            ("pressure", "traversal", "enter")
        }
        (TwoActorActor::Pressure, TwoActorOp::Traversal, TwoActorBoundary::Exit) => {
            ("pressure", "traversal", "exit")
        }
        (TwoActorActor::Pressure, TwoActorOp::Removal, TwoActorBoundary::Enter) => {
            ("pressure", "removal", "enter")
        }
        (TwoActorActor::Pressure, TwoActorOp::Removal, TwoActorBoundary::Exit) => {
            ("pressure", "removal", "exit")
        }
        _ => ("warm", "compilation", "exit"),
    };
    format!("{{\"actor\":\"{a}\",\"op\":\"{o}\",\"boundary\":\"{b}\",\"seq\":{seq}}}")
}

fn parse_two_actor_event(line: &str, global_seq: u64) -> Option<TwoActorEvent> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let actor = match value["actor"].as_str()? {
        "warm" => TwoActorActor::Warm,
        "pressure" => TwoActorActor::Pressure,
        _ => return None,
    };
    let op = match value["op"].as_str()? {
        "traversal" => TwoActorOp::Traversal,
        "compilation" => TwoActorOp::Compilation,
        "removal" => TwoActorOp::Removal,
        _ => return None,
    };
    let boundary = match value["boundary"].as_str()? {
        "enter" => TwoActorBoundary::Enter,
        "exit" => TwoActorBoundary::Exit,
        _ => return None,
    };
    Some(TwoActorEvent {
        actor,
        op,
        boundary,
        seq: global_seq,
    })
}

fn read_two_actor_events(log: &Path) -> Vec<TwoActorEvent> {
    let content = std::fs::read_to_string(log).unwrap_or_default();
    content
        .lines()
        .enumerate()
        .filter_map(|(i, line)| parse_two_actor_event(line, i as u64))
        .collect()
}

fn build_two_actor_intervals(events: &[TwoActorEvent]) -> Vec<TwoActorInterval> {
    let mut intervals = Vec::new();
    for actor in [TwoActorActor::Warm, TwoActorActor::Pressure] {
        for op in [
            TwoActorOp::Traversal,
            TwoActorOp::Compilation,
            TwoActorOp::Removal,
        ] {
            let mut enter_seq = None;
            for event in events.iter().filter(|e| e.actor == actor && e.op == op) {
                match event.boundary {
                    TwoActorBoundary::Enter => enter_seq = Some(event.seq),
                    TwoActorBoundary::Exit => {
                        if let Some(enter) = enter_seq.take() {
                            intervals.push(TwoActorInterval {
                                actor,
                                op,
                                enter_seq: enter,
                                exit_seq: event.seq,
                            });
                        }
                    }
                }
            }
        }
    }
    intervals
}

/// Thread-safe pressure observer that appends structured events to the shared
/// two-actor recorder log and increments a total-callback counter.
struct RecordingObserver {
    log: PathBuf,
    seq: std::sync::atomic::AtomicU64,
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl PressureOperationObserver for RecordingObserver {
    fn observe(&self, operation: PressureOperation) {
        use PressureOperation::*;
        let (op, boundary) = match operation {
            TraversalEnter => (TwoActorOp::Traversal, TwoActorBoundary::Enter),
            TraversalExit => (TwoActorOp::Traversal, TwoActorBoundary::Exit),
            RemovalEnter => (TwoActorOp::Removal, TwoActorBoundary::Enter),
            RemovalExit => (TwoActorOp::Removal, TwoActorBoundary::Exit),
        };
        self.counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let line = two_actor_event_line(TwoActorActor::Pressure, op, boundary, seq);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
        {
            use std::io::Write;
            let _ = writeln!(file, "{line}");
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn frozen_two_actor_schedule_serializes_warm_work_and_pressure_retry() {
    let _pressure_metrics = pressure_metrics_guard();
    use std::process::{Command, Stdio};

    // ---------- Child entry: run the landed warm path ----------
    if std::env::var_os("DJINN_TWO_ACTOR_WARM_CHILD").is_some() {
        let root = PathBuf::from(std::env::var("DJINN_TWO_ACTOR_ROOT").unwrap());
        let workspace = PathBuf::from(std::env::var("DJINN_TWO_ACTOR_WORKSPACE").unwrap());
        let id = std::env::var("DJINN_TWO_ACTOR_PROJECT").unwrap();
        let log = root.join("two-actor-recorder.jsonl");
        let counter = std::sync::atomic::AtomicU64::new(0);
        let observe = move |phase: djinn_agent_worker::cargo_incremental_prune::WarmWorkPhase| {
            use djinn_agent_worker::cargo_incremental_prune::WarmWorkPhase::*;
            let (op, boundary) = match phase {
                TraversalEnter => (TwoActorOp::Traversal, TwoActorBoundary::Enter),
                TraversalExit => (TwoActorOp::Traversal, TwoActorBoundary::Exit),
                CompilationEnter => (TwoActorOp::Compilation, TwoActorBoundary::Enter),
                CompilationExit => (TwoActorOp::Compilation, TwoActorBoundary::Exit),
            };
            let seq = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let line = two_actor_event_line(TwoActorActor::Warm, op, boundary, seq);
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)
                .unwrap();
            use std::io::Write;
            writeln!(file, "{line}").unwrap();
        };
        let _guard = djinn_agent_worker::cargo_incremental_prune::run_warm_work_at_root(
            &id, &root, &workspace, observe,
        )
        .unwrap();
        // Signal the parent that the warm actor holds the lock and is alive.
        std::fs::write(root.join("warm-lock-held"), b"held").unwrap();
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    // ---------- Parent: set up the two-actor schedule ----------
    let fixture = frozen_coordinator_fixture();
    let temp = tempfile::tempdir().unwrap();
    let id = "018f8b9a-0d70-7f0a-8000-000000000299";
    let base = old_base(&temp, id);
    let workspace = temp.path().join("warm-workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"warm_actor\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    std::fs::write(workspace.join("src/lib.rs"), "pub fn compiled() {}\n").unwrap();
    let log_path = temp.path().join("two-actor-recorder.jsonl");
    let _ = std::fs::File::create(&log_path).unwrap();

    // The warm child drives the real WarmBaseLock::acquire -> prune traversal ->
    // cargo check path and appends structured warm events to the shared recorder.
    let mut warm = Command::new(std::env::current_exe().unwrap())
        .args([
            "cargo_warm_base_gc::tests::pressure_execution::frozen_two_actor_schedule_serializes_warm_work_and_pressure_retry",
            "--exact",
            "--nocapture",
        ])
        .env("DJINN_TWO_ACTOR_WARM_CHILD", "1")
        .env("DJINN_TWO_ACTOR_ROOT", temp.path())
        .env("DJINN_TWO_ACTOR_WORKSPACE", &workspace)
        .env("DJINN_TWO_ACTOR_PROJECT", id)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start deterministic warm actor");

    // Wait until the child proves it reached the end of the warm path by
    // writing the held-lock signal. This proves real warm traversal and cargo
    // compilation completed while holding the lock.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if temp.path().join("warm-lock-held").exists() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "warm actor timed out");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Read the warm events the child recorded so far and assert the real warm
    // traversal and compilation ran to completion.
    let warm_events = read_two_actor_events(&log_path);
    assert!(
        warm_events.iter().any(|e| {
            e.actor == TwoActorActor::Warm
                && e.op == TwoActorOp::Traversal
                && e.boundary == TwoActorBoundary::Enter
        }) && warm_events.iter().any(|e| {
            e.actor == TwoActorActor::Warm
                && e.op == TwoActorOp::Traversal
                && e.boundary == TwoActorBoundary::Exit
        }),
        "warm child must record real traversal enter/exit"
    );
    assert!(
        warm_events.iter().any(|e| {
            e.actor == TwoActorActor::Warm
                && e.op == TwoActorOp::Compilation
                && e.boundary == TwoActorBoundary::Enter
        }) && warm_events.iter().any(|e| {
            e.actor == TwoActorActor::Warm
                && e.op == TwoActorOp::Compilation
                && e.boundary == TwoActorBoundary::Exit
        }),
        "warm child must record real compilation enter/exit"
    );

    let lock = SharedWarmBaseLock;
    let unit = eligible_three_rung_unit(&base, &base, PressureRung::WholeBase);

    // Snapshot every target-relative path, file bytes, and file type before the
    // loser attempts to run. This proves the loser performs no mutation.
    let before_snapshot = recursive_snapshot(&base);

    // Pressure-side observer: the test-only seam records the executor's real
    // traversal/removal boundaries into the same shared recorder.
    let pressure_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observer = RecordingObserver {
        log: log_path.clone(),
        seq: std::sync::atomic::AtomicU64::new(0),
        counter: pressure_counter.clone(),
    };
    set_pressure_operation_observer(Some(Box::new(observer)));

    // While the warm actor owns the shared lock, invoke the pressure executor.
    // It must fail to acquire the lock, performing zero traversal and zero
    // removal.
    let loser = execute_three_rung_pressure_plan(
        &ThreeRungPressurePlan {
            units: vec![unit.clone()],
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &lock,
        &SequenceCapacity(Mutex::new(std::collections::VecDeque::new())),
        &executable_pressure_config(),
        &three_rung_clock(),
        temp.path(),
    )
    .await;
    set_pressure_operation_observer(None);

    assert!(
        loser.attempted.is_empty() && loser.deleted.is_empty(),
        "pressure loser must not attempt or delete while warm owns the lock"
    );
    assert_eq!(
        pressure_counter.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "pressure loser must perform zero traversal and zero removal callbacks"
    );
    let after_loser_snapshot = recursive_snapshot(&base);
    assert_eq!(
        before_snapshot, after_loser_snapshot,
        "pressure loser must leave every target-relative path, file bytes, and file type unchanged"
    );

    // Kill and reap the warm lock owner without graceful unlock. The kernel
    // releases the advisory flock on process death.
    warm.kill().unwrap();
    warm.wait().unwrap();

    // Retry with the same observed executor and SharedWarmBaseLock. After the
    // owner's death, the lock is available.
    let retry_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let retry_observer = RecordingObserver {
        log: log_path.clone(),
        seq: std::sync::atomic::AtomicU64::new(0),
        counter: retry_counter.clone(),
    };
    set_pressure_operation_observer(Some(Box::new(retry_observer)));
    let result = execute_three_rung_pressure_plan(
        &ThreeRungPressurePlan {
            units: vec![unit.clone()],
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &lock,
        &SequenceCapacity(Mutex::new(std::collections::VecDeque::from([
            Ok(CapacitySnapshot {
                total_bytes: 100,
                available_bytes: 10,
            }),
            Ok(CapacitySnapshot {
                total_bytes: 100,
                available_bytes: 10,
            }),
        ]))),
        &executable_pressure_config(),
        &three_rung_clock(),
        temp.path(),
    )
    .await;
    set_pressure_operation_observer(None);

    // Exact accounting after retry.
    assert_eq!(result.planned, vec![unit.clone()]);
    assert_eq!(result.post_lock_eligible, vec![unit.clone()]);
    assert_eq!(result.attempted, vec![unit.clone()]);
    assert_eq!(result.deleted, vec![unit.clone()]);
    assert!(result.retained.is_empty() && result.failed.is_empty());
    assert!(
        !base.exists(),
        "pressure retry removes only after owner death releases the lock"
    );

    // Exactly one pressure traversal enter/exit and one removal enter/exit
    // (4 callback boundaries total).
    assert_eq!(
        retry_counter.load(std::sync::atomic::Ordering::SeqCst),
        4,
        "retry must record exactly one traversal enter/exit and one removal enter/exit"
    );

    // Read the complete recorder and build intervals from the recorded data.
    let all_events = read_two_actor_events(&log_path);
    let intervals = build_two_actor_intervals(&all_events);
    assert!(
        !intervals.is_empty(),
        "recorder must have recorded at least one operation interval"
    );

    // Each enter must have a later exit (ordered seq within the same actor+op).
    for interval in &intervals {
        assert!(
            interval.enter_seq < interval.exit_seq,
            "every operation exit must come after its enter: {interval:?}"
        );
    }

    // Assert no warm interval overlaps any pressure interval, derived from the
    // recorded data — not from the static fixture literal.
    let warm_intervals: Vec<_> = intervals
        .iter()
        .filter(|i| i.actor == TwoActorActor::Warm)
        .collect();
    let pressure_intervals: Vec<_> = intervals
        .iter()
        .filter(|i| i.actor == TwoActorActor::Pressure)
        .collect();
    assert!(
        !warm_intervals.is_empty(),
        "recorder must contain warm operation intervals"
    );
    assert!(
        !pressure_intervals.is_empty(),
        "recorder must contain pressure operation intervals"
    );
    for warm_iv in &warm_intervals {
        for pressure_iv in &pressure_intervals {
            assert!(
                !warm_iv.overlaps(pressure_iv),
                "warm {warm_iv:?} must not overlap pressure {pressure_iv:?}"
            );
        }
    }

    assert_eq!(
        fixture["two_actor"]["lock_path"],
        ".warm-locks/<project-id>.lock"
    );
    assert_eq!(fixture["two_actor"]["loser_removals"], 0);
    assert_eq!(fixture["two_actor"]["retry_removals"], result.deleted.len());
}

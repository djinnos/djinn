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
        &Activity(Ok(snapshot())),
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

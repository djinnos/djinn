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

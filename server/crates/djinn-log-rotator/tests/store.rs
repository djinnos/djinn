use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use djinn_log_rotator::{
    Clock, ContainerName, FilesystemCapacity, GzipCompressor, LogStore, Namespace, PodUid,
    StoreConfig, StoreError, StreamIdentity,
};
use flate2::read::GzDecoder;
use serde_json::json;
use tempfile::tempdir;
use time::{OffsetDateTime, macros::datetime};

#[derive(Clone)]
struct FixedCapacity(Arc<Mutex<(u64, u64)>>);
impl FixedCapacity {
    fn new(total: u64, available: u64) -> Self {
        Self(Arc::new(Mutex::new((total, available))))
    }
    fn set_available(&self, available: u64) {
        self.0.lock().unwrap().1 = available;
    }
}
impl FilesystemCapacity for FixedCapacity {
    fn available_bytes(&self, _: &std::path::Path) -> std::io::Result<u64> {
        Ok(self.0.lock().unwrap().1)
    }
    fn total_bytes(&self, _: &std::path::Path) -> std::io::Result<u64> {
        Ok(self.0.lock().unwrap().0)
    }
}

struct FixedClock(Mutex<OffsetDateTime>);
impl FixedClock {
    fn new(time: OffsetDateTime) -> Self {
        Self(Mutex::new(time))
    }
}

mod log_store {
    use super::*;

    #[test]
    fn seven_day_boundary() {
        let root = tempdir().unwrap();
        const GIB: u64 = 1024 * 1024 * 1024;
        // Ten streams × seven sidecars model exactly 1 GiB/stream/day and
        // 10 GiB aggregate/day without allocating 70 GiB of physical data.
        for stream in 0..10 {
            let dir = root
                .path()
                .join(format!("ns-{stream}/pod-{stream}/container"));
            fs::create_dir_all(&dir).unwrap();
            for day in 17..24 {
                let gzip = dir.join(format!("202607{day:02}T120000Z-000000.jsonl.gz"));
                fs::write(&gzip, []).unwrap();
                fs::write(format!("{}.bytes", gzip.display()), format!("{GIB}\n")).unwrap();
            }
        }
        let log = store_at(
            root.path(),
            128 * 1024 * 1024,
            datetime!(2026-07-24 12:00 UTC),
        );
        log.append(&stream(), &json!({"message":"boundary"}))
            .unwrap();
        assert!(
            log.eviction_transitions()
                .iter()
                .all(|transition| !matches!(
                    transition.reason,
                    djinn_log_rotator::EvictionReason::Age
                        | djinn_log_rotator::EvictionReason::StreamQuota
                        | djinn_log_rotator::EvictionReason::GlobalQuota
                ))
        );
    }
}
impl Clock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        *self.0.lock().unwrap()
    }
}
fn stream() -> StreamIdentity {
    StreamIdentity::new(
        Namespace::new("prod").unwrap(),
        PodUid::new("550e8400-e29b-41d4-a716-446655440000").unwrap(),
        ContainerName::new("api").unwrap(),
    )
}
type TestStore = LogStore<FixedClock, GzipCompressor, FixedCapacity>;

fn store(root: &std::path::Path, bytes: u64) -> TestStore {
    store_at(root, bytes, datetime!(2026-07-23 12:00 UTC))
}
fn store_at(root: &std::path::Path, bytes: u64, time: OffsetDateTime) -> TestStore {
    store_with_config(
        root,
        StoreConfig {
            max_logical_bytes: bytes,
            ..StoreConfig::default()
        },
        time,
        FixedCapacity::new(1 << 50, 1 << 50),
    )
}
fn store_with_config(
    root: &std::path::Path,
    config: StoreConfig,
    time: OffsetDateTime,
    capacity: FixedCapacity,
) -> TestStore {
    LogStore::with_parts_and_capacity(
        root,
        config,
        FixedClock::new(time),
        GzipCompressor,
        capacity,
    )
    .unwrap()
}
fn fixture_segment(
    root: &std::path::Path,
    id: &StreamIdentity,
    hour: &str,
    bytes: u64,
    active: bool,
) -> std::path::PathBuf {
    let dir = root
        .join(id.namespace.as_str())
        .join(id.pod_uid.as_str())
        .join(id.container.as_str());
    fs::create_dir_all(&dir).unwrap();
    let suffix = if active { "active" } else { "gz" };
    let path = dir.join(format!("{hour}-000000.jsonl.{suffix}"));
    fs::write(&path, []).unwrap();
    fs::write(format!("{}.bytes", path.display()), format!("{bytes}\n")).unwrap();
    path
}
fn alternate_stream() -> StreamIdentity {
    StreamIdentity::new(
        Namespace::new("other").unwrap(),
        PodUid::new("550e8400-e29b-41d4-a716-446655440001").unwrap(),
        ContainerName::new("worker").unwrap(),
    )
}

#[test]
fn appends_complete_lines_with_modes_and_logical_sidecar() {
    let root = tempdir().unwrap();
    let log = store(root.path(), 100);
    let id = stream();
    let first = log.append(&id, &json!({"message":"a"})).unwrap();
    let second = log.append(&id, &json!({"message":"b"})).unwrap();
    let dir = log.stream_path(&id);
    let active = fs::read_dir(&dir)
        .unwrap()
        .map(Result::unwrap)
        .find(|e| e.file_name().to_string_lossy().ends_with(".active"))
        .unwrap()
        .path();
    assert_eq!(fs::read_to_string(&active).unwrap().lines().count(), 2);
    assert_eq!(
        fs::read_to_string(format!("{}.bytes", active.display()))
            .unwrap()
            .trim()
            .parse::<u64>()
            .unwrap(),
        first + second
    );
    for path in [
        root.path().to_path_buf(),
        root.path().join("prod"),
        root.path()
            .join("prod/550e8400-e29b-41d4-a716-446655440000"),
        dir.clone(),
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o750
        );
    }
    assert_eq!(
        fs::metadata(&active).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[test]
fn threshold_rotates_in_order_and_keeps_line_integrity() {
    let root = tempdir().unwrap();
    let log = store(root.path(), 20);
    let id = stream();
    log.append(&id, &json!({"message":"first"})).unwrap();
    log.append(&id, &json!({"message":"second"})).unwrap();
    let names: Vec<_> = fs::read_dir(log.stream_path(&id))
        .unwrap()
        .map(Result::unwrap)
        .map(|e| e.file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".gz") || n.ends_with(".active"))
        .collect();
    assert!(names.iter().any(|n| n.contains("-000000.jsonl.gz")));
    assert!(names.iter().any(|n| n.contains("-000001.jsonl.active")));
}

#[test]
fn hour_boundary_rotates_to_the_next_hour() {
    let root = tempdir().unwrap();
    let id = stream();
    store(root.path(), 1000)
        .append(&id, &json!({"message":"noon"}))
        .unwrap();
    let log = store_at(root.path(), 1000, datetime!(2026-07-23 13:00 UTC));
    log.append(&id, &json!({"message":"one pm"})).unwrap();
    let names: Vec<_> = fs::read_dir(log.stream_path(&id))
        .unwrap()
        .map(Result::unwrap)
        .map(|entry| entry.file_name().into_string().unwrap())
        .collect();
    assert!(
        names
            .iter()
            .any(|name| name == "20260723T120000Z-000000.jsonl.gz")
    );
    assert!(
        names
            .iter()
            .any(|name| name == "20260723T130000Z-000000.jsonl.active")
    );
}

#[test]
fn gzip_sidecar_tracks_logical_bytes_not_compressed_size() {
    let root = tempdir().unwrap();
    let id = stream();
    let record = json!({"message": "x".repeat(512)});
    let logical = serde_json::to_vec(&record).unwrap().len() as u64 + 1;
    let log = store(root.path(), logical + 1);
    log.append(&id, &record).unwrap();
    log.append(&id, &record).unwrap();
    let gzip = fs::read_dir(log.stream_path(&id))
        .unwrap()
        .map(Result::unwrap)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "gz"))
        .unwrap();
    let sidecar = fs::read_to_string(format!("{}.bytes", gzip.display()))
        .unwrap()
        .trim()
        .parse::<u64>()
        .unwrap();
    assert_eq!(sidecar, logical);
    assert_ne!(sidecar, fs::metadata(&gzip).unwrap().len());
    let mut content = Vec::new();
    GzDecoder::new(fs::File::open(gzip).unwrap())
        .read_to_end(&mut content)
        .unwrap();
    assert_eq!(content.len() as u64, logical);
}

#[test]
fn recovery_resumes_active_and_completes_closed_transition() {
    let root = tempdir().unwrap();
    let log = store(root.path(), 1000);
    let id = stream();
    log.append(&id, &json!({"message":"complete"})).unwrap();
    let dir = log.stream_path(&id);
    let active = fs::read_dir(&dir)
        .unwrap()
        .map(Result::unwrap)
        .find(|e| e.file_name().to_string_lossy().ends_with(".active"))
        .unwrap()
        .path();
    fs::OpenOptions::new()
        .append(true)
        .open(&active)
        .unwrap()
        .write_all(b"{partial")
        .unwrap();
    log.recover_stream(&id).unwrap();
    assert_eq!(
        fs::read_to_string(&active).unwrap(),
        "{\"message\":\"complete\"}\n"
    );
    fs::rename(&active, active.with_extension("closed")).unwrap();
    let closed = active.with_extension("closed");
    fs::rename(
        format!("{}.bytes", active.display()),
        format!("{}.bytes", closed.display()),
    )
    .unwrap();
    log.recover_stream(&id).unwrap();
    log.recover_stream(&id).unwrap();
    assert!(
        fs::read_dir(&dir)
            .unwrap()
            .map(Result::unwrap)
            .any(|e| e.file_name().to_string_lossy().ends_with(".jsonl.gz"))
    );
}

#[test]
fn startup_recovery_completes_each_compression_interruption_idempotently() {
    let root = tempdir().unwrap();
    let log = store(root.path(), 1000);
    let id = stream();
    log.append(&id, &json!({"message":"complete"})).unwrap();
    let dir = log.stream_path(&id);
    let active = fs::read_dir(&dir)
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.file_name().to_string_lossy().ends_with(".active"))
        .unwrap()
        .path();
    let closed = active.with_extension("closed");
    fs::rename(&active, &closed).unwrap();
    fs::rename(
        format!("{}.bytes", active.display()),
        format!("{}.bytes", closed.display()),
    )
    .unwrap();

    // Restart from a durable `.closed` source.
    let _restart = store(root.path(), 1000);
    let gzip = closed.with_extension("gz");
    assert!(gzip.exists());

    // A renamed-but-not-unlinked temp is promoted when its source is absent.
    let temporary = std::path::PathBuf::from(format!("{}.tmp", gzip.display()));
    fs::rename(&gzip, &temporary).unwrap();
    let _restart = store(root.path(), 1000);
    assert!(gzip.exists());
    assert!(!temporary.exists());

    // If both source and temp survived, the source is recompressed and temp discarded.
    fs::write(&closed, b"{\"message\":\"complete\"}\n").unwrap();
    fs::copy(&gzip, &temporary).unwrap();
    let _restart = store(root.path(), 1000);
    assert!(gzip.exists());
    assert!(!closed.exists());
    assert!(!temporary.exists());

    // A completed rename with a stale `.closed` source is also safe to repeat.
    fs::write(&closed, b"{\"message\":\"complete\"}\n").unwrap();
    let _restart = store(root.path(), 1000);
    assert!(gzip.exists());
    assert!(!closed.exists());
}

#[test]
fn recovery_preserves_sidecars_across_both_rename_crash_windows() {
    let root = tempdir().unwrap();
    let log = store(root.path(), 1000);
    let id = stream();
    let expected = log.append(&id, &json!({"message":"complete"})).unwrap();
    let dir = log.stream_path(&id);
    let active = fs::read_dir(&dir)
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.file_name().to_string_lossy().ends_with(".active"))
        .unwrap()
        .path();
    let closed = active.with_extension("closed");

    // Crash after the segment rename, before `.active.bytes -> .closed.bytes`.
    fs::rename(&active, &closed).unwrap();
    store(root.path(), 1000);
    let gzip = closed.with_extension("gz");
    assert_eq!(
        fs::read_to_string(format!("{}.bytes", gzip.display()))
            .unwrap()
            .trim(),
        expected.to_string()
    );
    assert!(!std::path::PathBuf::from(format!("{}.bytes", active.display())).exists());
    assert!(!std::path::PathBuf::from(format!("{}.bytes", closed.display())).exists());
    store(root.path(), 1000);

    // Crash after unlinking `.closed`, before `.closed.bytes -> .gz.bytes`.
    let gzip_sidecar = std::path::PathBuf::from(format!("{}.bytes", gzip.display()));
    let closed_sidecar = std::path::PathBuf::from(format!("{}.bytes", closed.display()));
    fs::rename(&gzip_sidecar, &closed_sidecar).unwrap();
    store(root.path(), 1000);
    assert_eq!(
        fs::read_to_string(&gzip_sidecar).unwrap().trim(),
        expected.to_string()
    );
    assert!(!closed_sidecar.exists());
    store(root.path(), 1000);
    assert_eq!(
        fs::read_to_string(&gzip_sidecar).unwrap().trim(),
        expected.to_string()
    );
}

#[test]
fn age_evicts_only_segments_older_than_the_exact_boundary() {
    let root = tempdir().unwrap();
    let id = stream();
    let old = fixture_segment(root.path(), &id, "20260716T120000Z", 10, false);
    let boundary = fixture_segment(root.path(), &id, "20260717T120000Z", 10, false);
    let log = store_at(root.path(), 1000, datetime!(2026-07-24 12:00 UTC));
    log.append(&id, &json!({"message":"age"})).unwrap();
    assert!(!old.exists());
    assert!(boundary.exists());
    assert_eq!(
        log.eviction_transitions()[0].reason,
        djinn_log_rotator::EvictionReason::Age
    );
}

#[test]
fn stream_quota_evicts_oldest_closed_before_newer_active() {
    let root = tempdir().unwrap();
    let id = stream();
    let closed = fixture_segment(root.path(), &id, "20260720T120000Z", 100, false);
    let active = fixture_segment(root.path(), &id, "20260723T120000Z", 5, true);
    let log = store_with_config(
        root.path(),
        StoreConfig {
            max_logical_bytes: 1000,
            max_stream_logical_bytes: 100,
            max_global_logical_bytes: 1000,
            ..StoreConfig::default()
        },
        datetime!(2026-07-23 12:00 UTC),
        FixedCapacity::new(1 << 50, 1 << 50),
    );
    log.append(&id, &json!({"message":"quota"})).unwrap();
    assert!(!closed.exists());
    assert!(active.exists());
    assert_eq!(
        log.eviction_transitions()[0].reason,
        djinn_log_rotator::EvictionReason::StreamQuota
    );
}

#[test]
fn global_quota_evicts_oldest_closed_across_streams() {
    let root = tempdir().unwrap();
    let first = stream();
    let second = alternate_stream();
    let old = fixture_segment(root.path(), &first, "20260720T120000Z", 100, false);
    let newer = fixture_segment(root.path(), &second, "20260721T120000Z", 100, false);
    let log = store_with_config(
        root.path(),
        StoreConfig {
            max_logical_bytes: 1000,
            max_stream_logical_bytes: 1000,
            max_global_logical_bytes: 200,
            ..StoreConfig::default()
        },
        datetime!(2026-07-23 12:00 UTC),
        FixedCapacity::new(1 << 50, 1 << 50),
    );
    log.append(&second, &json!({"message":"global"})).unwrap();
    assert!(!old.exists());
    assert!(newer.exists());
    assert_eq!(
        log.eviction_transitions()[0].reason,
        djinn_log_rotator::EvictionReason::GlobalQuota
    );
}

#[test]
fn quota_rotates_an_active_segment_only_when_no_closed_segment_exists() {
    let root = tempdir().unwrap();
    let id = stream();
    let active = fixture_segment(root.path(), &id, "20260720T120000Z", 100, true);
    // Active-sidecar recovery validates against complete physical lines.
    fs::write(&active, format!("{}\n", "x".repeat(99))).unwrap();
    let log = store_with_config(
        root.path(),
        StoreConfig {
            max_logical_bytes: 1000,
            max_stream_logical_bytes: 100,
            max_global_logical_bytes: 1000,
            ..StoreConfig::default()
        },
        datetime!(2026-07-23 12:00 UTC),
        FixedCapacity::new(1 << 50, 1 << 50),
    );
    log.append(&id, &json!({"message":"rotate"})).unwrap();
    assert!(!active.exists());
    assert!(
        log.eviction_transitions()
            .iter()
            .any(|t| t.reason == djinn_log_rotator::EvictionReason::StreamQuota)
    );
}

#[test]
fn reserve_enter_stay_exit_rejects_without_mutation_and_recovers_writability() {
    let root = tempdir().unwrap();
    let id = stream();
    let capacity = FixedCapacity::new(2_000, 99);
    let log = store_with_config(
        root.path(),
        StoreConfig {
            minimum_reserve_bytes: 100,
            reserve_percent: 10,
            ..StoreConfig::default()
        },
        datetime!(2026-07-23 12:00 UTC),
        capacity.clone(),
    );
    assert!(matches!(
        log.append(&id, &json!({"message":"nope"})),
        Err(StoreError::ReserveExhausted)
    ));
    assert!(!log.stream_path(&id).exists());
    let state = log.writable_state().unwrap();
    assert!(!state.writable);
    assert_eq!(state.required_reserve_bytes, 200);
    assert_eq!(log.reserve_transitions().len(), 1);
    assert!(matches!(
        log.append(&id, &json!({"message":"still nope"})),
        Err(StoreError::ReserveExhausted)
    ));
    assert_eq!(log.reserve_transitions().len(), 1);
    capacity.set_available(200);
    assert!(log.writable_state().unwrap().writable);
    assert_eq!(log.reserve_transitions().len(), 2);
    assert!(log.reserve_transitions()[1].state.writable);
    log.append(&id, &json!({"message":"accepted"})).unwrap();
}

#[test]
fn restart_uses_logical_sidecars_for_quota_accounting() {
    let root = tempdir().unwrap();
    let id = stream();
    let retained = fixture_segment(root.path(), &id, "20260720T120000Z", 100, false);
    let config = StoreConfig {
        max_logical_bytes: 1000,
        max_stream_logical_bytes: 100,
        max_global_logical_bytes: 1000,
        ..StoreConfig::default()
    };
    let log = store_with_config(
        root.path(),
        config,
        datetime!(2026-07-23 12:00 UTC),
        FixedCapacity::new(1 << 50, 1 << 50),
    );
    log.append(&id, &json!({"message":"restart quota"}))
        .unwrap();
    assert!(!retained.exists());
    assert_eq!(log.eviction_transitions()[0].logical_bytes, 100);
}

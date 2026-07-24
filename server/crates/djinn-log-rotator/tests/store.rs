use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;

use djinn_log_rotator::{
    Clock, ContainerName, LogStore, Namespace, PodUid, StoreConfig, StreamIdentity,
};
use serde_json::json;
use tempfile::tempdir;
use time::{OffsetDateTime, macros::datetime};

struct FixedClock(Mutex<OffsetDateTime>);
impl FixedClock {
    fn new(time: OffsetDateTime) -> Self {
        Self(Mutex::new(time))
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
fn store(root: &std::path::Path, bytes: u64) -> LogStore<FixedClock> {
    LogStore::with_parts(
        root,
        StoreConfig {
            max_logical_bytes: bytes,
        },
        FixedClock::new(datetime!(2026-07-23 12:00 UTC)),
        Default::default(),
    )
    .unwrap()
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
    assert_eq!(
        fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
        0o750
    );
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

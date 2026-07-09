use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use djinn_core::clock::{Clock, SystemClock as SystemClockTrait};
use tokio::sync::{Mutex, RwLock};

type SessionId = String;
type NormalizedPath = String;
type SessionFileTimes = HashMap<NormalizedPath, ReadRecord>;
type FileTimeMap = HashMap<SessionId, SessionFileTimes>;

/// What portion of a file was observed by a read.
///
/// Used by downstream GateGuard checks to decide whether a planned
/// edit/write/patch targets a span the caller has actually seen. This module
/// records the coverage but does **not** enforce any gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadCoverage {
    /// The entire file was read end-to-end.
    Full,
    /// A contiguous byte range `[start, end)` was read.
    ///
    /// `start` is the inclusive lower byte offset (from the beginning of the
    /// file). `end` is the **exclusive** upper byte offset; `None` means the
    /// range extends to the end of the file at the time of reading (e.g. an
    /// offset read that ran to EOF without a `has_more` window boundary).
    Range {
        start: u64,
        end: Option<u64>,
    },
}

/// A single recorded read of a file within a session.
///
/// Replaces the former opaque `(SystemTime, Option<SystemTime>)` tuple,
/// preserving `read_at` and `modified_at_when_read` while adding coverage
/// metadata and a truncation flag for downstream GateGuard checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRecord {
    /// Wall-clock time the read was recorded.
    pub read_at: SystemTime,
    /// The file's modification time observed at read time (`None` if the file
    /// did not exist).
    pub modified_at_when_read: Option<SystemTime>,
    /// What portion of the file the read covered.
    pub coverage: ReadCoverage,
    /// `true` when the read was truncated by a byte/line budget and therefore
    /// did not observe the complete requested span. Used by GateGuard
    /// diagnostics/rate-limiting downstream; not enforced in this module.
    pub truncated: bool,
}

impl ReadRecord {
    /// Whether this record represents a full-file read (`ReadCoverage::Full`).
    pub fn is_full(&self) -> bool {
        matches!(self.coverage, ReadCoverage::Full)
    }

    /// Whether the recorded coverage fully contains the byte span
    /// `[span_start, span_end)`.
    ///
    /// A `Full` read covers any span. A `Range { start, end }` read covers the
    /// span only when `span_start >= start` and the span's end is within `end`
    /// (or `end` is `None`, meaning the range extended to EOF at read time).
    pub fn covers_span(&self, span_start: u64, span_end: u64) -> bool {
        match self.coverage {
            ReadCoverage::Full => true,
            ReadCoverage::Range { start, end } => {
                span_start >= start && end.is_none_or(|e| span_end <= e)
            }
        }
    }
}

#[derive(Default)]
pub struct FileTime {
    // session_id -> (normalized_path -> ReadRecord)
    inner: RwLock<FileTimeMap>,
    // Per-file write locks to serialize concurrent writes to the same file
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl FileTime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a **full-file** read of `path` in this session.
    ///
    /// This is a compatibility wrapper equivalent to
    /// [`read_with_coverage`](Self::read_with_coverage) with
    /// [`ReadCoverage::Full`] and `truncated = false`. Existing callers that
    /// treat every read as full-file should continue using this method until
    /// migrated.
    pub async fn read(&self, session_id: &str, path: &Path) -> Result<(), String> {
        self.read_with_coverage(session_id, path, ReadCoverage::Full, false)
            .await
    }

    /// Record a read of `path` in this session with explicit coverage metadata.
    ///
    /// `coverage` describes the portion of the file actually observed (full or
    /// a byte range), and `truncated` flags reads cut short by a byte/line
    /// budget. Downstream GateGuard code uses these fields to decide whether a
    /// planned edit targets a span the caller has seen; this method itself
    /// performs no enforcement.
    pub async fn read_with_coverage(
        &self,
        session_id: &str,
        path: &Path,
        coverage: ReadCoverage,
        truncated: bool,
    ) -> Result<(), String> {
        let normalized = normalize(path);
        let now = SystemClockTrait::new().now();
        let mtime = file_mtime(path)?;
        let record = ReadRecord {
            read_at: now,
            modified_at_when_read: mtime,
            coverage,
            truncated,
        };
        let mut guard = self.inner.write().await;
        let by_path = guard.entry(session_id.to_string()).or_default();
        by_path.insert(normalized, record);
        Ok(())
    }

    /// Forget any recorded read for `path` in this session, so the next
    /// `assert` falls into the "must be read before modification" path and
    /// forces the model to re-read before editing again.
    ///
    /// Called after a successful modify (write/edit/apply_patch): the model's
    /// in-context view of the file is now stale relative to what's on disk, so
    /// any further edit MUST re-read first. We invalidate explicitly rather
    /// than relying on the post-write mtime advancing past the recorded mtime,
    /// because coarse mtime granularity (or a same-tick write) could otherwise
    /// leave `current_mtime == read_mtime` and silently let a stale chained
    /// edit through.
    pub async fn invalidate(&self, session_id: &str, path: &Path) {
        let normalized = normalize(path);
        let mut guard = self.inner.write().await;
        if let Some(by_path) = guard.get_mut(session_id) {
            by_path.remove(&normalized);
        }
    }

    pub async fn get(&self, session_id: &str, path: &Path) -> Option<SystemTime> {
        let normalized = normalize(path);
        let guard = self.inner.read().await;
        guard
            .get(session_id)
            .and_then(|m| m.get(&normalized).map(|rec| rec.read_at))
    }

    /// Return the latest recorded read record for `path` in this session, if
    /// one exists. Downstream handlers use this to inspect coverage/truncation
    /// metadata for GateGuard checks without enforcing behavior here.
    pub async fn latest_record(&self, session_id: &str, path: &Path) -> Option<ReadRecord> {
        let normalized = normalize(path);
        let guard = self.inner.read().await;
        guard
            .get(session_id)
            .and_then(|m| m.get(&normalized).copied())
    }

    pub async fn assert(&self, session_id: &str, path: &Path) -> Result<(), String> {
        let normalized = normalize(path);
        let record = {
            let guard = self.inner.read().await;
            guard
                .get(session_id)
                .and_then(|m| m.get(&normalized).copied())
                .ok_or_else(|| {
                    format!(
                        "file must be read before modification in this session: {}",
                        path.display()
                    )
                })?
        };

        let current_mtime = file_mtime(path)?;
        if current_mtime != record.modified_at_when_read {
            return Err(format!(
                "file was modified since last read in this session: {} (last_read={:?})",
                path.display(),
                record.read_at
            ));
        }
        Ok(())
    }

    /// Acquire a per-file mutex, then execute the given future while holding
    /// the lock.  This serializes concurrent writes to the same file path,
    /// preventing race conditions when multiple agent tasks target the same
    /// file simultaneously.
    pub async fn with_lock<F, T>(&self, path: &Path, f: F) -> T
    where
        F: Future<Output = T>,
    {
        let canonical = canonical_lock_key(path);
        let mutex = {
            let mut map = self.locks.lock().await;
            Arc::clone(
                map.entry(canonical)
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let _guard = mutex.lock().await;
        f.await
    }
}

/// Produce a stable key for the per-file lock map.  We try to canonicalize
/// first so that symlinks / `..` segments resolve to the same entry; if the
/// file doesn't exist yet we fall back to the raw path.
fn canonical_lock_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn normalize(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn file_mtime(path: &Path) -> Result<Option<SystemTime>, String> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(meta.modified().ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("failed to read file metadata: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Explicitly set a file's modification time, bypassing coarse filesystem
    /// mtime granularity so modified-since-read assertions are deterministic.
    fn set_file_mtime(path: &Path, mtime: SystemTime) {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for set_times");
        let times = std::fs::FileTimes::new().set_modified(mtime);
        f.set_times(times).expect("set_times");
    }

    #[tokio::test]
    async fn full_file_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello").unwrap();

        let ft = FileTime::new();
        ft.read("s1", &path).await.unwrap();

        let rec = ft.latest_record("s1", &path).await.unwrap();
        assert!(rec.is_full(), "read() should record full coverage");
        assert!(rec.covers_span(0, 5), "full read covers any span");
        assert!(!rec.truncated, "compatibility read() is never truncated");
    }

    #[tokio::test]
    async fn partial_coverage_bounded_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.txt");
        std::fs::write(&path, "0123456789").unwrap();

        let ft = FileTime::new();
        ft.read_with_coverage(
            "s1",
            &path,
            ReadCoverage::Range {
                start: 2,
                end: Some(7),
            },
            false,
        )
        .await
        .unwrap();

        let rec = ft.latest_record("s1", &path).await.unwrap();
        assert!(!rec.is_full(), "range read is not full");
        assert!(rec.covers_span(2, 5), "span inside range");
        assert!(rec.covers_span(3, 7), "span reaching exclusive end boundary");
        assert!(
            !rec.covers_span(0, 3),
            "span starting before range is not covered"
        );
        assert!(
            !rec.covers_span(5, 8),
            "span ending past range end is not covered"
        );
    }

    #[tokio::test]
    async fn partial_coverage_open_range_to_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.txt");
        std::fs::write(&path, "0123456789").unwrap();

        let ft = FileTime::new();
        ft.read_with_coverage(
            "s1",
            &path,
            ReadCoverage::Range { start: 4, end: None },
            false,
        )
        .await
        .unwrap();

        let rec = ft.latest_record("s1", &path).await.unwrap();
        assert!(!rec.is_full(), "open-ended range is not full");
        assert!(
            rec.covers_span(4, 100),
            "open-ended range (to EOF) covers any span at or after its start"
        );
        assert!(
            !rec.covers_span(0, 4),
            "span before the range start is not covered"
        );
    }

    #[tokio::test]
    async fn truncated_flag_handling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.txt");
        std::fs::write(&path, "x".repeat(100)).unwrap();

        let ft = FileTime::new();
        ft.read_with_coverage(
            "s1",
            &path,
            ReadCoverage::Range {
                start: 0,
                end: Some(50),
            },
            true,
        )
        .await
        .unwrap();

        let rec = ft.latest_record("s1", &path).await.unwrap();
        assert!(rec.truncated, "truncated flag should be preserved");
        assert!(!rec.is_full());
    }

    #[tokio::test]
    async fn missing_read_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "data").unwrap();

        let ft = FileTime::new();
        let err = ft.assert("s1", &path).await.unwrap_err();
        assert!(
            err.starts_with("file must be read before modification in this session:"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn modified_since_read_failure_includes_read_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "initial").unwrap();

        let ft = FileTime::new();
        ft.read("s1", &path).await.unwrap();

        // Capture the recorded read time so we can assert it appears in the error.
        let recorded_read_at = ft.get("s1", &path).await.unwrap();

        // Simulate an external modification: change mtime to something distinct.
        let new_mtime = SystemTime::now() + Duration::from_secs(120);
        set_file_mtime(&path, new_mtime);

        let err = ft.assert("s1", &path).await.unwrap_err();
        assert!(
            err.starts_with("file was modified since last read in this session:"),
            "unexpected error: {err}"
        );
        assert!(
            err.contains("last_read="),
            "error should include last_read marker: {err}"
        );
        assert!(
            err.contains(&format!("{:?}", recorded_read_at)),
            "error should include the recorded read_at timestamp: {err}"
        );
    }

    #[tokio::test]
    async fn invalidation_clears_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.txt");
        std::fs::write(&path, "data").unwrap();

        let ft = FileTime::new();
        ft.read("s1", &path).await.unwrap();

        // Record exists before invalidation.
        assert!(ft.latest_record("s1", &path).await.is_some());
        assert!(ft.get("s1", &path).await.is_some());

        ft.invalidate("s1", &path).await;

        // Record cleared; subsequent assert hits the missing-read path.
        assert!(ft.latest_record("s1", &path).await.is_none());
        assert!(ft.get("s1", &path).await.is_none());
        let err = ft.assert("s1", &path).await.unwrap_err();
        assert!(
            err.starts_with("file must be read before modification in this session:"),
            "unexpected error after invalidation: {err}"
        );
    }

    #[tokio::test]
    async fn full_read_then_unchanged_mtime_passes_assert() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.txt");
        std::fs::write(&path, "stable").unwrap();

        let ft = FileTime::new();
        ft.read("s1", &path).await.unwrap();

        // Without changing mtime, assert should succeed.
        ft.assert("s1", &path).await.expect("unchanged mtime passes");
    }

    #[tokio::test]
    async fn sessions_are_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.txt");
        std::fs::write(&path, "shared").unwrap();

        let ft = FileTime::new();
        ft.read("session-a", &path).await.unwrap();

        // session-b has no record for this path.
        assert!(ft.latest_record("session-b", &path).await.is_none());
        let err = ft.assert("session-b", &path).await.unwrap_err();
        assert!(err.starts_with("file must be read before modification in this session:"));

        // session-a still has its record.
        assert!(ft.latest_record("session-a", &path).await.is_some());
    }
}

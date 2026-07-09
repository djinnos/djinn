use std::collections::{HashMap, HashSet};
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
type GateGuardMap = HashMap<SessionId, GateGuardSessionState>;

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
    Range { start: u64, end: Option<u64> },
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

/// Stable identifier for a class of destructive shell command that the
/// worker shell policy soft-gates on first invocation.
///
/// This is colocated with `FileTime` because the bash soft-gate set is keyed
/// per-session alongside the read records. The shape intentionally mirrors a
/// future richer taxonomy (sibling epic `c570` will provide a production
/// classifier that returns outcomes equivalent to `HardDeny`,
/// `SoftGate(DestructiveClass)`, or `Allow`); for now callers construct
/// values directly via the `const` items in [`destructive_class`] so
/// downstream state-tracking tests can exercise the per-class behavior
/// without depending on the classifier itself.
///
/// Equality and hashing are by label: two `DestructiveClass` values with the
/// same label compare equal and belong to the same bucket in a `HashSet`.
/// The inner reference is always `'static`; dynamic categories from a future
/// classifier are interned via [`DestructiveClass::from_owned`], which leaks
/// the label so it can be borrowed for the program's lifetime. That leak is
/// intentional and bounded — it runs once per category per process, not per
/// invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DestructiveClass(&'static str);

impl DestructiveClass {
    /// Borrow the underlying stable label.
    pub fn as_str(&self) -> &'static str {
        self.0
    }

    /// Intern an owned label into a `&'static str` and return a
    /// `DestructiveClass` backed by it. The string is leaked; callers should
    /// only use this for categories known to be a small, bounded set
    /// (typically sourced from a classifier's enum output).
    pub fn from_owned(label: String) -> Self {
        Self(Box::leak(label.into_boxed_str()))
    }
}

/// Known soft-gate categories the colocated helper state ships with.
///
/// These are intentionally minimal: sibling epic `c570` owns the
/// production-safe classifier that decides which real-world shell commands
/// map to each class. The helpers in `FileTime` only need a stable
/// identifier per category so the per-session `bash_soft_forced` set can
/// answer "has the worker already produced a FORCE plan for this class?"
/// deterministically. New categories added here must use a stable, uniquely
/// namespaced string label so values produced by future code paths remain
/// comparable to values produced today.
pub mod destructive_class {
    use super::DestructiveClass;

    /// Worktree-local file mutation that the worker must justify with a
    /// rollback/recreation plan on first use (e.g. `rm`, `mv` of untracked
    /// files inside the task worktree, excluding `.git` and durable project
    /// data).
    pub const WORKTREE_LOCAL_FILE_MUTATION: DestructiveClass =
        DestructiveClass("destructive.worktree_local_file_mutation");

    /// Soft-gated VCS mutation (e.g. non-destructive in-tree worktree-local
    /// operations the c570 classifier may carve out as soft-gated rather than
    /// hard-denied).
    pub const VCS_SOFT_GATE: DestructiveClass = DestructiveClass("destructive.vcs_soft_gate");

    /// Soft-gated database mutation (e.g. ad-hoc inserts/updates inside a
    /// scratch/dev DB the classifier designates as reworkable).
    pub const DB_SOFT_GATE: DestructiveClass = DestructiveClass("destructive.db_soft_gate");
}

/// Per-session GateGuard state colocated with the read records.
///
/// Each field is diagnostic-only state for downstream GateGuard behavior;
/// this struct itself performs **no enforcement** and is not exposed as an
/// edit allow token. The shape exists so later epics (`0inv` for edit
/// FORCE, `c570` for shell soft-gates) can read consistent session-scoped
/// bookkeeping without re-implementing it.
#[derive(Debug, Default, Clone)]
pub struct GateGuardSessionState {
    /// Normalized paths for which the ordinary first-edit investigation
    /// FORCE has already fired in this live session. Once a path is present
    /// here, subsequent edits to it no longer require another FORCE.
    pub edit_forced: HashSet<NormalizedPath>,
    /// Normalized paths for which the worker has already been shown the
    /// "truncated read" diagnostic in this live session. This is a
    /// diagnostic/rate-limiting signal only; it is **not** an edit allow
    /// token — sibling epic `0inv` owns the actual denial/prompt decision
    /// around truncated reads.
    pub truncated_forced: HashSet<NormalizedPath>,
    /// Destructive shell classes for which the worker has already produced a
    /// FORCE plan (rollback/recreation) in this live session. Once a class
    /// is present, subsequent invocations of the same class do not re-prompt.
    pub bash_soft_forced: HashSet<DestructiveClass>,
}

#[derive(Default)]
pub struct FileTime {
    // session_id -> (normalized_path -> ReadRecord)
    inner: RwLock<FileTimeMap>,
    // session_id -> colocated GateGuard diagnostic/session state.
    // Keyed by the same live session identity as `inner` so a single
    // diagnostic helper call touches both maps with the same key, but kept
    // as a separate top-level lock so the read-path hot loop never touches
    // GateGuard bookkeeping.
    gateguard: RwLock<GateGuardMap>,
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

    // ─── Colocated GateGuard diagnostic/session-state helpers ─────────────
    //
    // The helpers below operate exclusively on the colocated
    // `GateGuardSessionState` keyed by the same `session_id` used for read
    // records. Each helper is intentionally a thin, non-enforcing
    // mark/check/reset operation:
    //
    // * `mark_*` records state the next epic will act on.
    // * `has_*` reports whether state is already recorded.
    // * `reset_*` clears state explicitly. None of these helpers touch
    //   [`FileTime::read`], [`FileTime::invalidate`], or [`FileTime::assert`]:
    //   the read/invalidate hot path is preserved unchanged, and a
    //   concurrent `invalidate` will not silently clear GateGuard
    //   diagnostic state.
    //
    // Sibling epics own enforcement wiring; this epic only exposes the
    // reusable in-memory state.

    /// Mark `path` (under `session_id`) as having had its ordinary
    /// first-edit investigation FORCE fire in this live session. Idempotent.
    pub async fn mark_edit_forced(&self, session_id: &str, path: &Path) {
        let normalized = normalize(path);
        let mut guard = self.gateguard.write().await;
        let state = guard.entry(session_id.to_string()).or_default();
        state.edit_forced.insert(normalized);
    }

    /// Whether the ordinary first-edit investigation FORCE has already
    /// fired for `path` in this live session.
    pub async fn has_edit_forced(&self, session_id: &str, path: &Path) -> bool {
        let normalized = normalize(path);
        let guard = self.gateguard.read().await;
        guard
            .get(session_id)
            .is_some_and(|s| s.edit_forced.contains(&normalized))
    }

    /// Clear the first-edit FORCE flag for `path` in this live session, if
    /// present. Does not touch `truncated_forced` or `bash_soft_forced`.
    pub async fn reset_edit_forced(&self, session_id: &str, path: &Path) {
        let normalized = normalize(path);
        let mut guard = self.gateguard.write().await;
        if let Some(state) = guard.get_mut(session_id) {
            state.edit_forced.remove(&normalized);
        }
    }

    /// Mark `path` (under `session_id`) as having already been shown the
    /// "truncated read" diagnostic in this live session. Idempotent.
    ///
    /// **This is a diagnostic/rate-limiting signal only.** It is not exposed
    /// as an edit allow token; sibling epic `0inv` owns the decision around
    /// whether truncated reads permit edits. Recording it here lets the
    /// eventual denial/prompt message avoid repeating itself within a single
    /// session.
    pub async fn mark_truncated_forced(&self, session_id: &str, path: &Path) {
        let normalized = normalize(path);
        let mut guard = self.gateguard.write().await;
        let state = guard.entry(session_id.to_string()).or_default();
        state.truncated_forced.insert(normalized);
    }

    /// Whether the "truncated read" diagnostic has already been shown for
    /// `path` in this live session.
    pub async fn has_truncated_forced(&self, session_id: &str, path: &Path) -> bool {
        let normalized = normalize(path);
        let guard = self.gateguard.read().await;
        guard
            .get(session_id)
            .is_some_and(|s| s.truncated_forced.contains(&normalized))
    }

    /// Clear the "truncated read" diagnostic flag for `path` in this live
    /// session, if present. Does not touch `edit_forced` or
    /// `bash_soft_forced`.
    pub async fn reset_truncated_forced(&self, session_id: &str, path: &Path) {
        let normalized = normalize(path);
        let mut guard = self.gateguard.write().await;
        if let Some(state) = guard.get_mut(session_id) {
            state.truncated_forced.remove(&normalized);
        }
    }

    /// Mark `class` (under `session_id`) as having already produced a
    /// FORCE plan (rollback/recreation) in this live session. Subsequent
    /// invocations of the same class will not re-prompt. Idempotent.
    pub async fn mark_bash_soft_forced(&self, session_id: &str, class: DestructiveClass) {
        let mut guard = self.gateguard.write().await;
        let state = guard.entry(session_id.to_string()).or_default();
        state.bash_soft_forced.insert(class);
    }

    /// Whether the worker has already produced a FORCE plan for `class` in
    /// this live session.
    pub async fn has_bash_soft_forced(&self, session_id: &str, class: &DestructiveClass) -> bool {
        let guard = self.gateguard.read().await;
        guard
            .get(session_id)
            .is_some_and(|s| s.bash_soft_forced.contains(class))
    }

    /// Clear the bash-soft-forced flag for `class` in this live session,
    /// if present. Does not touch `edit_forced` or `truncated_forced`.
    pub async fn reset_bash_soft_forced(&self, session_id: &str, class: &DestructiveClass) {
        let mut guard = self.gateguard.write().await;
        if let Some(state) = guard.get_mut(session_id) {
            state.bash_soft_forced.remove(class);
        }
    }

    /// Drop all colocated GateGuard state for `session_id`. Test-only and
    /// for explicit session-end hooks: this is **not** called by
    /// [`FileTime::invalidate`], and a normal `[read, invalidate, read]`
    /// sequence leaves the GateGuard state untouched so diagnostic and
    /// steady-state edit helpers cannot regress.
    pub async fn reset_all_gateguard_for_session(&self, session_id: &str) {
        let mut guard = self.gateguard.write().await;
        guard.remove(session_id);
    }

    /// Return a clone of the entire per-session GateGuard state, primarily
    /// for tests and diagnostics. Production enforcement code should use
    /// the targeted `has_*` helpers instead of inspecting this snapshot.
    pub async fn gateguard_snapshot(&self, session_id: &str) -> GateGuardSessionState {
        let guard = self.gateguard.read().await;
        guard.get(session_id).cloned().unwrap_or_default()
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
        assert!(
            rec.covers_span(3, 7),
            "span reaching exclusive end boundary"
        );
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
            ReadCoverage::Range {
                start: 4,
                end: None,
            },
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
    #[allow(clippy::disallowed_methods)] // test-only: real time to simulate an external mtime change
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
        ft.assert("s1", &path)
            .await
            .expect("unchanged mtime passes");
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

    // ─── Colocated GateGuard helper-state tests ─────────────────────────────

    fn touch_file(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "x").unwrap();
        path
    }

    #[tokio::test]
    async fn edit_forced_mark_and_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = touch_file(dir.path(), "edit.txt");

        let ft = FileTime::new();
        assert!(
            !ft.has_edit_forced("s1", &path).await,
            "fresh state should not report edit-forced"
        );

        ft.mark_edit_forced("s1", &path).await;
        assert!(
            ft.has_edit_forced("s1", &path).await,
            "mark should be observable via has_*"
        );

        // Idempotent: re-marking does not flip or fail.
        ft.mark_edit_forced("s1", &path).await;
        assert!(ft.has_edit_forced("s1", &path).await);
    }

    #[tokio::test]
    async fn truncated_forced_is_separate_from_edit_forced() {
        let dir = tempfile::tempdir().unwrap();
        let path = touch_file(dir.path(), "trunc.txt");

        let ft = FileTime::new();

        // Recording truncated_forced MUST NOT mark edit_forced.
        ft.mark_truncated_forced("s1", &path).await;
        assert!(ft.has_truncated_forced("s1", &path).await);
        assert!(
            !ft.has_edit_forced("s1", &path).await,
            "truncated_forced must not imply edit_forced"
        );

        // And the reverse: recording edit_forced MUST NOT mark truncated_forced.
        ft.mark_edit_forced("s1", &path).await;
        assert!(ft.has_edit_forced("s1", &path).await);
        assert!(
            ft.has_truncated_forced("s1", &path).await,
            "earlier truncated_forced is unaffected by the edit_forced mark"
        );

        // Sanity-check the snapshot encodes separation.
        let snap = ft.gateguard_snapshot("s1").await;
        assert!(snap.edit_forced.contains(&super::normalize(&path)));
        assert!(snap.truncated_forced.contains(&super::normalize(&path)));
    }

    #[tokio::test]
    async fn bash_soft_forced_is_scoped_per_class() {
        let ft = FileTime::new();

        let file_class = destructive_class::WORKTREE_LOCAL_FILE_MUTATION;
        let vcs_class = destructive_class::VCS_SOFT_GATE;
        let db_class = destructive_class::DB_SOFT_GATE;

        assert!(!ft.has_bash_soft_forced("s1", &file_class).await);
        assert!(!ft.has_bash_soft_forced("s1", &vcs_class).await);
        assert!(!ft.has_bash_soft_forced("s1", &db_class).await);

        ft.mark_bash_soft_forced("s1", file_class).await;

        // Marking one class MUST NOT mark any other class.
        assert!(ft.has_bash_soft_forced("s1", &file_class).await);
        assert!(
            !ft.has_bash_soft_forced("s1", &vcs_class).await,
            "file-mutation mark must not imply vcs mark"
        );
        assert!(
            !ft.has_bash_soft_forced("s1", &db_class).await,
            "file-mutation mark must not imply db mark"
        );

        ft.mark_bash_soft_forced("s1", db_class).await;
        assert!(ft.has_bash_soft_forced("s1", &file_class).await);
        assert!(!ft.has_bash_soft_forced("s1", &vcs_class).await);
        assert!(ft.has_bash_soft_forced("s1", &db_class).await);
    }

    #[tokio::test]
    async fn gates_are_isolated_across_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = touch_file(dir.path(), "shared.txt");

        let ft = FileTime::new();

        // session-a marks every gate for the shared path/class.
        ft.mark_edit_forced("session-a", &path).await;
        ft.mark_truncated_forced("session-a", &path).await;
        ft.mark_bash_soft_forced("session-a", destructive_class::WORKTREE_LOCAL_FILE_MUTATION)
            .await;

        // session-b sees none of those marks for the same path/class.
        assert!(!ft.has_edit_forced("session-b", &path).await);
        assert!(!ft.has_truncated_forced("session-b", &path).await);
        assert!(
            !ft.has_bash_soft_forced(
                "session-b",
                &destructive_class::WORKTREE_LOCAL_FILE_MUTATION,
            )
            .await
        );

        // And the read records are likewise isolated — sanity check the
        // session-id semantics share the same key shape.
        ft.read("session-a", &path).await.unwrap();
        assert!(ft.latest_record("session-a", &path).await.is_some());
        assert!(ft.latest_record("session-b", &path).await.is_none());
    }

    #[tokio::test]
    async fn gates_are_scoped_per_path() {
        let dir = tempfile::tempdir().unwrap();
        let a = touch_file(dir.path(), "a.txt");
        let b = touch_file(dir.path(), "b.txt");

        let ft = FileTime::new();

        ft.mark_edit_forced("s1", &a).await;
        ft.mark_truncated_forced("s1", &a).await;

        // b is untouched.
        assert!(!ft.has_edit_forced("s1", &b).await);
        assert!(!ft.has_truncated_forced("s1", &b).await);
        // a still has both marks.
        assert!(ft.has_edit_forced("s1", &a).await);
        assert!(ft.has_truncated_forced("s1", &a).await);
    }

    #[tokio::test]
    async fn invalidate_does_not_clear_gateguard_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = touch_file(dir.path(), "race.txt");

        let ft = FileTime::new();

        // Build up reads and GateGuard marks for the same path.
        ft.read("s1", &path).await.unwrap();
        ft.mark_edit_forced("s1", &path).await;
        ft.mark_truncated_forced("s1", &path).await;
        ft.mark_bash_soft_forced("s1", destructive_class::WORKTREE_LOCAL_FILE_MUTATION)
            .await;

        // invalidate() removes the read record…
        ft.invalidate("s1", &path).await;
        assert!(ft.latest_record("s1", &path).await.is_none());

        // …but MUST NOT touch the colocated GateGuard diagnostic state. This
        // is a documented property; only the targeted `reset_*` helpers and
        // `reset_all_gateguard_for_session` may clear it.
        assert!(ft.has_edit_forced("s1", &path).await);
        assert!(ft.has_truncated_forced("s1", &path).await);
        assert!(
            ft.has_bash_soft_forced("s1", &destructive_class::WORKTREE_LOCAL_FILE_MUTATION,)
                .await
        );
    }

    #[tokio::test]
    async fn reset_helpers_clear_only_their_own_gate() {
        let dir = tempfile::tempdir().unwrap();
        let path = touch_file(dir.path(), "z.txt");

        let ft = FileTime::new();

        ft.mark_edit_forced("s1", &path).await;
        ft.mark_truncated_forced("s1", &path).await;
        ft.mark_bash_soft_forced("s1", destructive_class::WORKTREE_LOCAL_FILE_MUTATION)
            .await;

        // reset_edit_forced clears only the edit gate.
        ft.reset_edit_forced("s1", &path).await;
        assert!(!ft.has_edit_forced("s1", &path).await);
        assert!(ft.has_truncated_forced("s1", &path).await);
        assert!(
            ft.has_bash_soft_forced("s1", &destructive_class::WORKTREE_LOCAL_FILE_MUTATION,)
                .await
        );

        // reset_truncated_forced clears only the truncated gate.
        ft.reset_truncated_forced("s1", &path).await;
        assert!(!ft.has_truncated_forced("s1", &path).await);
        assert!(
            ft.has_bash_soft_forced("s1", &destructive_class::WORKTREE_LOCAL_FILE_MUTATION,)
                .await
        );

        // reset_bash_soft_forced clears only that class.
        ft.reset_bash_soft_forced("s1", &destructive_class::WORKTREE_LOCAL_FILE_MUTATION)
            .await;
        assert!(
            !ft.has_bash_soft_forced("s1", &destructive_class::WORKTREE_LOCAL_FILE_MUTATION,)
                .await
        );

        // reset on absent state is a no-op (does not create an empty entry).
        ft.reset_edit_forced("s1", &path).await; // path absent already
        ft.reset_edit_forced("never-touched", &path).await;
        let snap = ft.gateguard_snapshot("never-touched").await;
        assert!(snap.edit_forced.is_empty());
        assert!(snap.truncated_forced.is_empty());
        assert!(snap.bash_soft_forced.is_empty());
    }

    #[tokio::test]
    async fn reset_all_gateguard_for_session_wipes_only_one_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = touch_file(dir.path(), "w.txt");

        let ft = FileTime::new();

        for session in ["session-a", "session-b"] {
            ft.mark_edit_forced(session, &path).await;
            ft.mark_truncated_forced(session, &path).await;
            ft.mark_bash_soft_forced(session, destructive_class::VCS_SOFT_GATE)
                .await;
        }

        ft.reset_all_gateguard_for_session("session-a").await;

        // session-a is wiped.
        let snap_a = ft.gateguard_snapshot("session-a").await;
        assert!(snap_a.edit_forced.is_empty());
        assert!(snap_a.truncated_forced.is_empty());
        assert!(snap_a.bash_soft_forced.is_empty());

        // session-b still has every mark.
        assert!(ft.has_edit_forced("session-b", &path).await);
        assert!(ft.has_truncated_forced("session-b", &path).await);
        assert!(
            ft.has_bash_soft_forced("session-b", &destructive_class::VCS_SOFT_GATE)
                .await
        );
    }

    #[tokio::test]
    async fn destructive_class_consts_are_distinct_and_round_trip() {
        // Sanity-check the const taxonomy is non-collision-prone and stable.
        let file_class = destructive_class::WORKTREE_LOCAL_FILE_MUTATION;
        let vcs_class = destructive_class::VCS_SOFT_GATE;
        let db_class = destructive_class::DB_SOFT_GATE;

        assert_ne!(file_class, vcs_class);
        assert_ne!(file_class, db_class);
        assert_ne!(vcs_class, db_class);

        // Round-trip through from_owned via as_str — proves equality is by label.
        let roundtrip = DestructiveClass::from_owned(file_class.as_str().to_string());
        assert_eq!(roundtrip, file_class);

        // HashSet bucketing works (no false negatives from collisions).
        let mut set = HashSet::new();
        set.insert(file_class);
        set.insert(vcs_class);
        set.insert(db_class);
        set.insert(DestructiveClass::from_owned(
            destructive_class::WORKTREE_LOCAL_FILE_MUTATION
                .as_str()
                .to_string(),
        ));
        assert_eq!(set.len(), 3, "duplicates collapse by label");
    }
}

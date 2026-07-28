//! Retention bound for the on-disk SCIP indexer cache.
//!
//! # Why this exists
//!
//! [`crate::scip_indexer::cache::ScipCacheStore`] is content-addressed and
//! append-only: every distinct key mints a new entry directory and nothing
//! ever removed one. On the production cluster that tree had grown to 4.5 GiB
//! across 40 entries on the shared `/cache` PVC — the same claim that already
//! causes recurring node DiskPressure — with no prune, no TTL, and no cap.
//! Growth is proportional to warm frequency, so the tree is unbounded in time.
//!
//! # What "in use" means here
//!
//! Multiple warm Jobs and task-run Pods mount the same PVC concurrently, so a
//! sweep runs against a tree other processes are actively reading and writing.
//! Three independent layers keep an eviction from ever destroying live work:
//!
//! 1. **Publication lock.** A writer inside
//!    [`crate::scip_indexer::cache::ScipCacheStore::store_bytes`] holds the
//!    entry's [`PUBLISH_LOCK_DIR`] for the whole tmp-write-and-rename. An entry
//!    carrying that directory is never a candidate.
//! 2. **In-flight grace.** An entry whose newest access or modification time is
//!    within `RetentionPolicy::in_flight_grace` is never a candidate, which
//!    covers a reader that has opened the manifest but not yet the artifact,
//!    and any writer whose lock this sweep raced past.
//! 3. **Stage-then-delete.** An eviction `rename(2)`s the entry directory to a
//!    sibling `.<name>.evicting.<nanos>.<pid>.tmp` before `remove_dir_all`.
//!    The rename is atomic, so a concurrent lookup on that key sees either the
//!    complete entry or `ENOENT` — never a directory whose manifest survived
//!    its artifact. Both `ENOENT` shapes are already a plain cache miss on the
//!    read path, and a POSIX unlink cannot pull bytes out from under a reader
//!    that already has the file open. The `.tmp` suffix and pid qualifier match
//!    the [`crate::warm_sentinel`] convention for partial state.
//!
//! Sweeps are serialised across processes by a directory-based lock, and
//! throttled by a stamp file so a burst of stores does not rescan the tree
//! once per store.
//!
//! # Ordering
//!
//! Eviction is least-recently-*used*, not least-recently-written: an entry's
//! age is the newest of its files' `atime`/`mtime`, so an entry that keeps
//! getting cache hits keeps getting younger and survives. On a `noatime` mount
//! `atime` never advances and this degrades to least-recently-written, which
//! is still a bound — just a less selective one.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use djinn_core::clock::{Clock, SystemClock};

/// Marker file that identifies a directory as a cache entry. Single-sourced
/// with the writer in [`crate::scip_indexer::cache`].
pub(super) const ENTRY_MANIFEST: &str = "manifest.json";

/// Directory a publishing writer creates inside an entry for the duration of
/// its tmp-write-and-rename.
pub(super) const PUBLISH_LOCK_DIR: &str = ".publish.lock";

/// Cross-process mutex (a directory, so `mkdir` gives us the atomic test-and-set)
/// serialising sweeps over one cache root.
const SWEEP_LOCK_DIR: &str = ".retention.lock";

/// Stamp file whose mtime throttles how often a sweep may run.
const SWEEP_STAMP_FILE: &str = ".retention.stamp";

/// A sweep lock older than this is assumed to belong to a crashed process and
/// is reclaimed. Generous relative to a sweep, which is a directory walk.
const SWEEP_LOCK_STALE_AFTER: Duration = Duration::from_secs(3600);

/// Env override for the total-size ceiling, in bytes. `0` disables the size
/// leg (an explicit operator opt-out); absent or unparseable keeps the default.
pub(super) const MAX_BYTES_ENV: &str = "DJINN_SCIP_CACHE_MAX_BYTES";

/// Env override for the idle window, in hours. `0` disables the idle leg.
pub(super) const MAX_IDLE_HOURS_ENV: &str = "DJINN_SCIP_CACHE_MAX_IDLE_HOURS";

/// Default ceiling on the total size of the cache tree: 4 GiB.
///
/// Sized from the production measurement rather than a round guess. One warm
/// generation of the largest project on the cluster costs ~165 MiB across all
/// its workspaces (a ~154 MiB Rust index plus a ~11 MiB TypeScript one), and
/// warms land roughly hourly, so 4 GiB retains ~25 generations ≈ 25 h. Every
/// reuse actually observed on that tree re-read an entry written 0.8–26 h
/// earlier, so the cap sits above the real reuse distance and evicts only
/// entries that were already dead. It is also ~5% of the 76 GiB the shared
/// cache claim currently occupies, against the 4.5 GiB and climbing that an
/// unbounded tree had reached.
pub(super) const DEFAULT_MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Default idle window: 7 days.
///
/// The size cap is the binding constraint for a busy project. This leg exists
/// for the opposite case — a project warmed rarely, whose few entries would
/// otherwise sit on the PVC forever. Measured against *last use*, so an entry
/// that keeps hitting is never idle.
pub(super) const DEFAULT_MAX_IDLE: Duration = Duration::from_secs(7 * 24 * 3600);

/// Entries touched within this window are never evicted. Covers the widest
/// plausible in-flight read or publish; a 154 MiB artifact reads in seconds.
const DEFAULT_IN_FLIGHT_GRACE: Duration = Duration::from_secs(30 * 60);

/// Minimum wall-clock gap between sweeps over one root.
const DEFAULT_MIN_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Retention bounds applied to one cache root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RetentionPolicy {
    /// Total-size ceiling in bytes. `None` disables the size leg.
    pub max_total_bytes: Option<u64>,
    /// Maximum time since last use. `None` disables the idle leg.
    pub max_idle: Option<Duration>,
    /// Entries touched more recently than this are never candidates.
    pub in_flight_grace: Duration,
    /// Minimum gap between sweeps over one root.
    pub min_sweep_interval: Duration,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_total_bytes: Some(DEFAULT_MAX_TOTAL_BYTES),
            max_idle: Some(DEFAULT_MAX_IDLE),
            in_flight_grace: DEFAULT_IN_FLIGHT_GRACE,
            min_sweep_interval: DEFAULT_MIN_SWEEP_INTERVAL,
        }
    }
}

impl RetentionPolicy {
    /// Resolve the policy from the process environment.
    ///
    /// Both bounds are **on by default**: an absent, empty, or unparseable
    /// override leaves the shipped default in place. Only an explicit `0`
    /// disables a leg, and only the leg it names.
    pub(super) fn from_environment() -> Self {
        Self::from_env(|name| std::env::var(name).ok())
    }

    fn from_env(mut get_env: impl FnMut(&str) -> Option<String>) -> Self {
        let default = Self::default();
        let max_total_bytes = match parse_u64(get_env(MAX_BYTES_ENV)) {
            Some(0) => None,
            Some(bytes) => Some(bytes),
            None => default.max_total_bytes,
        };
        let max_idle = match parse_u64(get_env(MAX_IDLE_HOURS_ENV)) {
            Some(0) => None,
            Some(hours) => Some(Duration::from_secs(hours.saturating_mul(3600))),
            None => default.max_idle,
        };
        Self {
            max_total_bytes,
            max_idle,
            ..default
        }
    }
}

fn parse_u64(value: Option<String>) -> Option<u64> {
    value?.trim().parse::<u64>().ok()
}

/// What a sweep did. Every field is a counted side effect, not a label.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SweepReport {
    /// Entries considered.
    pub scanned: usize,
    /// Entries removed because they exceeded the idle window.
    pub evicted_idle: usize,
    /// Entries removed to bring the tree under the size ceiling.
    pub evicted_over_cap: usize,
    /// Total bytes reclaimed.
    pub reclaimed_bytes: u64,
    /// Bytes still on disk after the sweep.
    pub retained_bytes: u64,
    /// Candidates skipped because they were in flight (publish lock or grace).
    pub skipped_in_flight: usize,
    /// Staging directories left behind by a previously interrupted sweep.
    pub reclaimed_staged: usize,
}

impl SweepReport {
    pub(super) fn evicted(&self) -> usize {
        self.evicted_idle + self.evicted_over_cap
    }
}

/// Run a sweep if one is due and no other process is already sweeping.
///
/// Returns `None` when the sweep was throttled or the lock was held — both are
/// ordinary outcomes, not errors. Never returns an error: retention is
/// best-effort maintenance and must not fail a warm.
pub(super) fn maybe_enforce_retention(
    root: &Path,
    policy: &RetentionPolicy,
) -> Option<SweepReport> {
    let now = SystemClock::new().now();
    if !root.is_dir() {
        return None;
    }
    if !sweep_is_due(root, policy, now) {
        return None;
    }
    let _lock = SweepLock::try_acquire(root, now)?;
    // Stamp before sweeping so a crash mid-sweep still throttles the retry.
    write_stamp(root);
    Some(enforce_retention(root, policy, now))
}

/// Apply the retention policy to `root` as of `now`.
///
/// Exposed separately from [`maybe_enforce_retention`] so tests drive the
/// policy against an explicit clock instead of backdating files.
pub(super) fn enforce_retention(
    root: &Path,
    policy: &RetentionPolicy,
    now: SystemTime,
) -> SweepReport {
    let mut report = SweepReport::default();
    let mut entries = Vec::new();
    collect(root, &mut entries, &mut report);
    report.scanned = entries.len();

    // A candidate is an entry no one is publishing into and whose newest
    // access is outside the in-flight grace window.
    let (mut candidates, protected): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .partition(|entry| entry.is_evictable(now, policy.in_flight_grace));
    report.skipped_in_flight = protected.len();

    let mut live_bytes: u64 = protected.iter().map(|entry| entry.bytes).sum();

    // Least-recently-used first, so the survivors of a size trim are the
    // entries a subsequent run is most likely to hit.
    candidates.sort_by(|left, right| {
        left.last_used
            .cmp(&right.last_used)
            .then_with(|| left.dir.cmp(&right.dir))
    });

    let mut retained: Vec<CacheEntry> = Vec::with_capacity(candidates.len());
    for entry in candidates {
        let idle_expired = policy.max_idle.is_some_and(|max_idle| {
            now.duration_since(entry.last_used)
                .is_ok_and(|idle| idle > max_idle)
        });
        if idle_expired && evict(&entry) {
            report.evicted_idle += 1;
            report.reclaimed_bytes += entry.bytes;
            continue;
        }
        live_bytes += entry.bytes;
        retained.push(entry);
    }

    if let Some(cap) = policy.max_total_bytes {
        // `retained` is still in LRU order, so draining from the front trims
        // the coldest entries first.
        for entry in &retained {
            if live_bytes <= cap {
                break;
            }
            if evict(entry) {
                report.evicted_over_cap += 1;
                report.reclaimed_bytes += entry.bytes;
                live_bytes = live_bytes.saturating_sub(entry.bytes);
            }
        }
    }

    report.retained_bytes = live_bytes;
    report
}

/// One cache entry: a directory holding a manifest and its artifact.
#[derive(Debug, Clone)]
struct CacheEntry {
    dir: PathBuf,
    bytes: u64,
    /// Newest `atime`/`mtime` across the entry's files.
    last_used: SystemTime,
    /// A writer holds this entry's publication lock right now.
    publishing: bool,
}

impl CacheEntry {
    fn is_evictable(&self, now: SystemTime, grace: Duration) -> bool {
        if self.publishing {
            return false;
        }
        match now.duration_since(self.last_used) {
            // Clock skew across pods can put an entry "in the future"; treat
            // that as freshly touched rather than infinitely old.
            Err(_) => false,
            Ok(idle) => idle >= grace,
        }
    }
}

/// Walk `root`, appending every entry directory found and reclaiming staging
/// directories abandoned by an interrupted sweep.
///
/// Directories whose name begins with `.` are never entries: that covers the
/// publication lock, the sweep lock, and staged evictions.
fn collect(dir: &Path, out: &mut Vec<CacheEntry>, report: &mut SweepReport) {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };
    for child in read_dir.flatten() {
        if !child.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = child.path();
        let name = child.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            // Staged evictions only ever exist while the sweep lock is held,
            // so any we can see here belong to a sweep that died.
            if name.ends_with(".tmp") && fs::remove_dir_all(&path).is_ok() {
                report.reclaimed_staged += 1;
            }
            continue;
        }
        if path.join(ENTRY_MANIFEST).is_file() {
            if let Some(entry) = measure(&path) {
                out.push(entry);
            }
        } else {
            collect(&path, out, report);
        }
    }
}

fn measure(dir: &Path) -> Option<CacheEntry> {
    let read_dir = fs::read_dir(dir).ok()?;
    let mut bytes = 0u64;
    let mut last_used = SystemTime::UNIX_EPOCH;
    let mut publishing = false;
    for child in read_dir.flatten() {
        let Ok(kind) = child.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if child.file_name() == PUBLISH_LOCK_DIR {
                publishing = true;
            }
            continue;
        }
        let Ok(metadata) = child.metadata() else {
            continue;
        };
        bytes = bytes.saturating_add(metadata.len());
        for stamp in [metadata.modified().ok(), metadata.accessed().ok()]
            .into_iter()
            .flatten()
        {
            if stamp > last_used {
                last_used = stamp;
            }
        }
    }
    Some(CacheEntry {
        dir: dir.to_path_buf(),
        bytes,
        last_used,
        publishing,
    })
}

/// Remove one entry: rename it out of the keyspace, then delete the staged
/// copy. Returns whether the entry is gone from its key path.
fn evict(entry: &CacheEntry) -> bool {
    let Some(parent) = entry.dir.parent() else {
        return false;
    };
    let Some(name) = entry.dir.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let staged = parent.join(format!(
        ".{name}.evicting.{}.{}.tmp",
        unix_nanos(),
        std::process::id()
    ));
    match fs::rename(&entry.dir, &staged) {
        Ok(()) => {}
        // Another sweep or a manual clean-up got there first: the key path is
        // still gone, which is what the caller is asking about.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return true,
        Err(error) => {
            tracing::debug!(
                error = %error,
                entry = %entry.dir.display(),
                "scip cache retention: could not stage entry for eviction"
            );
            return false;
        }
    }
    if let Err(error) = fs::remove_dir_all(&staged) {
        // The entry is already unreachable by key; the staged directory will
        // be reclaimed by the next sweep's `collect`.
        tracing::debug!(
            error = %error,
            staged = %staged.display(),
            "scip cache retention: staged entry removal failed; will retry next sweep"
        );
    }
    true
}

fn sweep_is_due(root: &Path, policy: &RetentionPolicy, now: SystemTime) -> bool {
    let Ok(metadata) = fs::metadata(root.join(SWEEP_STAMP_FILE)) else {
        return true;
    };
    let Ok(modified) = metadata.modified() else {
        return true;
    };
    now.duration_since(modified)
        .is_ok_and(|since| since >= policy.min_sweep_interval)
}

fn write_stamp(root: &Path) {
    let path = root.join(SWEEP_STAMP_FILE);
    let temp = root.join(format!(
        ".{SWEEP_STAMP_FILE}.{}.{}.tmp",
        unix_nanos(),
        std::process::id()
    ));
    if fs::write(&temp, b"").is_ok() && fs::rename(&temp, &path).is_err() {
        let _ = fs::remove_file(&temp);
    }
}

/// Cross-process sweep mutex. `mkdir` is the atomic test-and-set; `Drop`
/// releases it.
struct SweepLock {
    path: PathBuf,
}

impl SweepLock {
    fn try_acquire(root: &Path, now: SystemTime) -> Option<Self> {
        let path = root.join(SWEEP_LOCK_DIR);
        match fs::create_dir(&path) {
            Ok(()) => return Some(Self { path }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(_) => return None,
        }
        // Reclaim a lock left behind by a process that died mid-sweep,
        // otherwise retention would stop forever after one crash.
        let stale = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|held| held > SWEEP_LOCK_STALE_AFTER);
        if !stale {
            return None;
        }
        if fs::remove_dir(&path).is_err() {
            return None;
        }
        fs::create_dir(&path).ok().map(|()| Self { path })
    }
}

impl Drop for SweepLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

fn unix_nanos() -> u128 {
    SystemClock::new()
        .now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("djinn-scip-retention-")
            .tempdir_in(".")
            .expect("create tempdir")
    }

    /// Materialise an entry at the real store layout: `v1/<shard>/<key>/`.
    fn write_entry(root: &Path, key: &str, artifact_bytes: usize) -> PathBuf {
        let dir = root.join("v1").join(&key[..2]).join(key);
        fs::create_dir_all(&dir).expect("create entry dir");
        fs::write(dir.join(ENTRY_MANIFEST), format!("{{\"key\":\"{key}\"}}"))
            .expect("write manifest");
        fs::write(dir.join("artifact.scip"), vec![b'x'; artifact_bytes]).expect("write artifact");
        dir
    }

    /// Set both atime and mtime on every file in an entry. Lets a test place
    /// entries at exact points on the LRU axis without sleeping.
    #[cfg(unix)]
    fn set_entry_times(dir: &Path, seconds_since_epoch: i64) {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        for child in fs::read_dir(dir).expect("read entry").flatten() {
            if !child.file_type().expect("file type").is_file() {
                continue;
            }
            let path = CString::new(child.path().as_os_str().as_bytes()).expect("path bytes");
            let times = [
                libc::timespec {
                    tv_sec: seconds_since_epoch,
                    tv_nsec: 0,
                },
                libc::timespec {
                    tv_sec: seconds_since_epoch,
                    tv_nsec: 0,
                },
            ];
            // SAFETY: `path` is a valid NUL-terminated path and `times` is a
            // two-element timespec array, exactly what `utimensat` expects.
            let rc = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) };
            assert_eq!(rc, 0, "utimensat failed for {}", child.path().display());
        }
    }

    fn at(seconds_since_epoch: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds_since_epoch)
    }

    fn exists(dir: &Path) -> bool {
        dir.join(ENTRY_MANIFEST).is_file()
    }

    // ------------------------------------------------------------------
    // The load-bearing test: entries that must go are GONE from disk and
    // entries that must stay are STILL THERE, with their bytes intact.
    //
    // Neutralisation check (documented in the PR): replacing the body of
    // `evict` with `true` — i.e. reporting the eviction without performing
    // it — turns the four `exists(...)` assertions below red, because they
    // stat the key path rather than reading the returned counters.
    // ------------------------------------------------------------------
    #[cfg(unix)]
    #[test]
    fn sweep_deletes_expired_and_over_cap_entries_and_keeps_the_live_ones() {
        let tmp = tempdir();
        let root = tmp.path();

        // Four entries, 1 KiB each, placed at distinct points on the LRU axis.
        let ancient = write_entry(root, "aa11111111111111", 1024);
        let cold = write_entry(root, "bb22222222222222", 1024);
        let warm = write_entry(root, "cc33333333333333", 1024);
        let hot = write_entry(root, "dd44444444444444", 1024);

        const DAY: u64 = 24 * 3600;
        set_entry_times(&ancient, (30 * DAY) as i64);
        set_entry_times(&cold, (100 * DAY) as i64);
        set_entry_times(&warm, (101 * DAY) as i64);
        set_entry_times(&hot, (102 * DAY) as i64);

        let entry_bytes = 1024
            + fs::read(hot.join(ENTRY_MANIFEST))
                .expect("read manifest")
                .len() as u64;
        let now = at(102 * DAY + 3600);
        let policy = RetentionPolicy {
            // `ancient` is 72 days idle; the rest are at most 2 days.
            max_idle: Some(Duration::from_secs(7 * DAY)),
            // Room for exactly two of the three entries that survive the idle
            // leg, so the size leg must trim exactly one more.
            max_total_bytes: Some(2 * entry_bytes),
            in_flight_grace: Duration::from_secs(600),
            min_sweep_interval: Duration::ZERO,
        };

        let report = enforce_retention(root, &policy, now);

        // Side effects on disk, not the report's own arithmetic.
        assert!(
            !exists(&ancient),
            "an entry {} days past the idle window must be gone from {}",
            72,
            ancient.display()
        );
        assert!(
            !exists(&cold),
            "the coldest survivor must be trimmed to satisfy the size cap: {}",
            cold.display()
        );
        assert!(
            exists(&warm),
            "an entry inside both bounds must survive: {}",
            warm.display()
        );
        assert!(
            exists(&hot),
            "the most recently used entry must survive: {}",
            hot.display()
        );
        // Survivors keep their bytes — eviction must not truncate neighbours.
        assert_eq!(
            fs::read(warm.join("artifact.scip"))
                .expect("read surviving artifact")
                .len(),
            1024
        );
        assert_eq!(
            fs::read(hot.join("artifact.scip"))
                .expect("read surviving artifact")
                .len(),
            1024
        );

        // And nothing is left staged in the keyspace.
        let shard = root.join("v1").join("bb");
        let leftovers = fs::read_dir(&shard)
            .expect("read shard")
            .flatten()
            .filter(|child| child.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0, "no staged eviction may remain visible");

        assert_eq!(report.scanned, 4);
        assert_eq!(report.evicted_idle, 1);
        assert_eq!(report.evicted_over_cap, 1);
        assert_eq!(report.reclaimed_bytes, 2 * entry_bytes);
        assert_eq!(report.retained_bytes, 2 * entry_bytes);
    }

    /// The size trim must be able to reach every unprotected entry, including
    /// the newest one, when the cap is smaller than a single entry. Without
    /// this the cap would be advisory for a cache whose entries are large.
    #[cfg(unix)]
    #[test]
    fn size_cap_smaller_than_one_entry_empties_the_unprotected_tree() {
        let tmp = tempdir();
        let root = tmp.path();
        let first = write_entry(root, "aa11111111111111", 4096);
        let second = write_entry(root, "bb22222222222222", 4096);
        set_entry_times(&first, 1_000_000);
        set_entry_times(&second, 1_000_100);

        let report = enforce_retention(
            root,
            &RetentionPolicy {
                max_idle: None,
                max_total_bytes: Some(1),
                in_flight_grace: Duration::from_secs(60),
                min_sweep_interval: Duration::ZERO,
            },
            at(1_000_200),
        );

        assert!(!exists(&first), "{} must be gone", first.display());
        assert!(!exists(&second), "{} must be gone", second.display());
        assert_eq!(report.evicted_over_cap, 2);
        assert_eq!(report.retained_bytes, 0);
    }

    /// Constraint: eviction must never remove an entry another process is
    /// publishing into. The publication lock is the writer's own signal.
    #[cfg(unix)]
    #[test]
    fn an_entry_being_published_survives_a_sweep_that_would_otherwise_evict_it() {
        let tmp = tempdir();
        let root = tmp.path();
        let entry = write_entry(root, "aa11111111111111", 4096);
        set_entry_times(&entry, 1_000);
        fs::create_dir(entry.join(PUBLISH_LOCK_DIR)).expect("hold publish lock");

        let report = enforce_retention(
            root,
            &RetentionPolicy {
                // Both legs would condemn this entry if the lock were ignored.
                max_idle: Some(Duration::from_secs(1)),
                max_total_bytes: Some(0),
                in_flight_grace: Duration::from_secs(60),
                min_sweep_interval: Duration::ZERO,
            },
            at(10_000_000),
        );

        assert!(
            exists(&entry),
            "a locked entry must survive: {}",
            entry.display()
        );
        assert_eq!(report.evicted(), 0);
        assert_eq!(report.skipped_in_flight, 1);
    }

    /// Constraint: an entry touched inside the grace window is assumed to be
    /// mid-read by another pod and is never a candidate.
    #[cfg(unix)]
    #[test]
    fn an_entry_touched_inside_the_grace_window_survives() {
        let tmp = tempdir();
        let root = tmp.path();
        let entry = write_entry(root, "aa11111111111111", 4096);
        set_entry_times(&entry, 1_000_000);

        let report = enforce_retention(
            root,
            &RetentionPolicy {
                max_idle: Some(Duration::from_secs(1)),
                max_total_bytes: Some(0),
                in_flight_grace: Duration::from_secs(3600),
                min_sweep_interval: Duration::ZERO,
            },
            // 10 minutes after its last use — inside the 1 h grace window.
            at(1_000_600),
        );

        assert!(
            exists(&entry),
            "an entry touched 10 min ago must survive: {}",
            entry.display()
        );
        assert_eq!(report.skipped_in_flight, 1);
    }

    /// A staging directory from a sweep that died between rename and delete
    /// must be reclaimed, and must never be mistaken for a live entry.
    #[test]
    fn a_staged_eviction_left_by_a_crashed_sweep_is_reclaimed() {
        let tmp = tempdir();
        let root = tmp.path();
        let live = write_entry(root, "aa11111111111111", 16);
        let staged = root.join("v1").join("bb").join(".bb2222.evicting.7.9.tmp");
        fs::create_dir_all(&staged).expect("create staged dir");
        fs::write(staged.join(ENTRY_MANIFEST), b"{}").expect("write staged manifest");

        let report = enforce_retention(
            root,
            &RetentionPolicy {
                max_idle: None,
                max_total_bytes: None,
                ..RetentionPolicy::default()
            },
            SystemClock::new().now(),
        );

        assert!(!staged.exists(), "{} must be reclaimed", staged.display());
        assert_eq!(report.reclaimed_staged, 1);
        assert_eq!(
            report.scanned, 1,
            "a staged directory must not be counted as a cache entry"
        );
        assert!(exists(&live));
    }

    /// Both bounds ship on. A deployment that sets nothing at all must still
    /// get a finite cache.
    #[test]
    fn defaults_are_on_without_any_environment_configuration() {
        let policy = RetentionPolicy::from_env(|_| None);
        assert_eq!(policy.max_total_bytes, Some(DEFAULT_MAX_TOTAL_BYTES));
        assert_eq!(policy.max_idle, Some(DEFAULT_MAX_IDLE));
        assert!(policy.max_total_bytes.expect("cap") > 0);
    }

    #[test]
    fn environment_overrides_each_leg_independently_and_zero_disables_it() {
        let sized = RetentionPolicy::from_env(|name| match name {
            MAX_BYTES_ENV => Some("123456".to_string()),
            _ => None,
        });
        assert_eq!(sized.max_total_bytes, Some(123_456));
        assert_eq!(sized.max_idle, Some(DEFAULT_MAX_IDLE));

        let aged = RetentionPolicy::from_env(|name| match name {
            MAX_IDLE_HOURS_ENV => Some("48".to_string()),
            _ => None,
        });
        assert_eq!(aged.max_idle, Some(Duration::from_secs(48 * 3600)));
        assert_eq!(aged.max_total_bytes, Some(DEFAULT_MAX_TOTAL_BYTES));

        let uncapped = RetentionPolicy::from_env(|name| match name {
            MAX_BYTES_ENV => Some("0".to_string()),
            _ => None,
        });
        assert_eq!(uncapped.max_total_bytes, None);
        assert_eq!(uncapped.max_idle, Some(DEFAULT_MAX_IDLE));

        // Garbage must not silently disable a bound.
        let garbage = RetentionPolicy::from_env(|name| match name {
            MAX_BYTES_ENV => Some("not-a-number".to_string()),
            MAX_IDLE_HOURS_ENV => Some(String::new()),
            _ => None,
        });
        assert_eq!(garbage.max_total_bytes, Some(DEFAULT_MAX_TOTAL_BYTES));
        assert_eq!(garbage.max_idle, Some(DEFAULT_MAX_IDLE));
    }

    /// The throttle must actually suppress the second sweep, and the lock must
    /// actually exclude a second sweeper.
    #[test]
    fn sweeps_are_throttled_by_the_stamp_and_serialised_by_the_lock() {
        let tmp = tempdir();
        let root = tmp.path();
        write_entry(root, "aa11111111111111", 16);
        let policy = RetentionPolicy {
            min_sweep_interval: Duration::from_secs(3600),
            ..RetentionPolicy::default()
        };

        assert!(maybe_enforce_retention(root, &policy).is_some());
        assert!(
            root.join(SWEEP_STAMP_FILE).is_file(),
            "the first sweep must leave a stamp"
        );
        assert!(
            maybe_enforce_retention(root, &policy).is_none(),
            "a sweep inside the throttle window must not run"
        );

        // Held lock, throttle satisfied: still no second sweeper.
        fs::remove_file(root.join(SWEEP_STAMP_FILE)).expect("clear stamp");
        let held = SweepLock::try_acquire(root, SystemTime::UNIX_EPOCH).expect("acquire lock");
        assert!(
            maybe_enforce_retention(root, &policy).is_none(),
            "a sweep must not run while another holds the lock"
        );
        drop(held);
        assert!(maybe_enforce_retention(root, &policy).is_some());
    }

    /// A lock orphaned by a crashed process must not disable retention forever.
    #[test]
    fn a_stale_sweep_lock_is_reclaimed() {
        let tmp = tempdir();
        let root = tmp.path();
        // Simulate a process that died holding the lock: the directory exists
        // with no owner alive to drop it.
        fs::create_dir(root.join(SWEEP_LOCK_DIR)).expect("orphan the lock");

        assert!(
            SweepLock::try_acquire(root, SystemClock::new().now()).is_none(),
            "a fresh lock must not be stolen"
        );
        let far_future = SystemClock::new().now() + SWEEP_LOCK_STALE_AFTER * 2;
        assert!(
            SweepLock::try_acquire(root, far_future).is_some(),
            "a lock held past the stale window must be reclaimed"
        );
    }
}

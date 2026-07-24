use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde_json::Value;
use thiserror::Error;
use time::{Duration, OffsetDateTime, UtcOffset};

use crate::StreamIdentity;

pub const DEFAULT_MAX_LOGICAL_BYTES: u64 = 128 * 1024 * 1024;
pub const DEFAULT_MAX_STREAM_LOGICAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const DEFAULT_MAX_GLOBAL_LOGICAL_BYTES: u64 = 100 * 1024 * 1024 * 1024;
pub const DEFAULT_MINIMUM_RESERVE_BYTES: u64 = 20 * 1024 * 1024 * 1024;
pub const DEFAULT_MAX_AGE: Duration = Duration::days(7);
const DIR_MODE: u32 = 0o750;
const FILE_MODE: u32 = 0o640;

pub trait Clock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}

pub trait FilesystemCapacity: Send + Sync {
    fn available_bytes(&self, root: &Path) -> io::Result<u64>;
    fn total_bytes(&self, root: &Path) -> io::Result<u64>;
}
#[derive(Debug, Default)]
pub struct SystemFilesystemCapacity;
impl FilesystemCapacity for SystemFilesystemCapacity {
    fn available_bytes(&self, root: &Path) -> io::Result<u64> {
        self.stat(root)
            .map(|s| s.f_bavail.saturating_mul(s.f_frsize))
    }
    fn total_bytes(&self, root: &Path) -> io::Result<u64> {
        self.stat(root)
            .map(|s| s.f_blocks.saturating_mul(s.f_frsize))
    }
}
impl SystemFilesystemCapacity {
    fn stat(&self, root: &Path) -> io::Result<libc::statvfs> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let path = CString::new(root.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in store path"))?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } == 0 {
            Ok(unsafe { stat.assume_init() })
        } else {
            Err(io::Error::last_os_error())
        }
    }
}
#[derive(Debug, Default)]
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

/// Compression is injectable so storage and recovery tests do not depend on wall time or gzip implementation details.
pub trait Compressor: Send + Sync {
    fn gzip(&self, source: &Path, temporary: &Path) -> io::Result<()>;
}
#[derive(Debug, Default)]
pub struct GzipCompressor;
impl Compressor for GzipCompressor {
    fn gzip(&self, source: &Path, temporary: &Path) -> io::Result<()> {
        let mut input = BufReader::new(File::open(source)?);
        let output = create_file(temporary)?;
        let mut gzip = GzEncoder::new(output, Compression::default());
        io::copy(&mut input, &mut gzip)?;
        let output = gzip.finish()?;
        output.sync_all()
    }
}

#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub max_logical_bytes: u64,
    pub max_stream_logical_bytes: u64,
    pub max_global_logical_bytes: u64,
    pub max_age: Duration,
    pub minimum_reserve_bytes: u64,
    pub reserve_percent: u8,
}
impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            max_logical_bytes: DEFAULT_MAX_LOGICAL_BYTES,
            max_stream_logical_bytes: DEFAULT_MAX_STREAM_LOGICAL_BYTES,
            max_global_logical_bytes: DEFAULT_MAX_GLOBAL_LOGICAL_BYTES,
            max_age: DEFAULT_MAX_AGE,
            minimum_reserve_bytes: DEFAULT_MINIMUM_RESERVE_BYTES,
            reserve_percent: 10,
        }
    }
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("record must be a JSON object")]
    NonObjectRecord,
    #[error("invalid active segment name: {0}")]
    InvalidSegmentName(String),
    #[error("the physical filesystem reserve is exhausted")]
    ReserveExhausted,
    #[error("retention quota cannot be satisfied without rewriting an active segment")]
    QuotaCannotBeSatisfied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictionReason {
    Age,
    StreamQuota,
    GlobalQuota,
    Reserve,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictionTransition {
    pub reason: EvictionReason,
    pub logical_bytes: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritableState {
    pub writable: bool,
    pub required_reserve_bytes: u64,
    pub available_bytes: u64,
}
/// A physical-reserve state change. `writable == false` marks reserve entry;
/// `writable == true` marks recovery after the reserve predicate clears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReserveTransition {
    pub state: WritableState,
}
#[derive(Clone)]
struct Segment {
    logical_bytes: u64,
    active: bool,
    path: PathBuf,
    key: String,
    dir: PathBuf,
}

pub struct LogStore<
    C: Clock = SystemClock,
    Z: Compressor = GzipCompressor,
    F: FilesystemCapacity = SystemFilesystemCapacity,
> {
    root: PathBuf,
    config: StoreConfig,
    clock: Arc<C>,
    compressor: Arc<Z>,
    capacity: Arc<F>,
    policy_lock: Mutex<()>,
    state: Mutex<WritableState>,
    transitions: Mutex<Vec<EvictionTransition>>,
    reserve_transitions: Mutex<Vec<ReserveTransition>>,
}
impl LogStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        Self::with_parts(root, StoreConfig::default(), SystemClock, GzipCompressor)
    }
}
impl<C: Clock, Z: Compressor> LogStore<C, Z, SystemFilesystemCapacity> {
    pub fn with_parts(
        root: impl Into<PathBuf>,
        config: StoreConfig,
        clock: C,
        compressor: Z,
    ) -> Result<Self, StoreError> {
        Self::with_parts_and_capacity(root, config, clock, compressor, SystemFilesystemCapacity)
    }
}
impl<C: Clock, Z: Compressor, F: FilesystemCapacity> LogStore<C, Z, F> {
    pub fn with_parts_and_capacity(
        root: impl Into<PathBuf>,
        config: StoreConfig,
        clock: C,
        compressor: Z,
        capacity: F,
    ) -> Result<Self, StoreError> {
        let root = root.into();
        create_dir(&root)?;
        let store = Self {
            root,
            config,
            clock: Arc::new(clock),
            compressor: Arc::new(compressor),
            capacity: Arc::new(capacity),
            policy_lock: Mutex::new(()),
            state: Mutex::new(WritableState {
                writable: true,
                required_reserve_bytes: 0,
                available_bytes: 0,
            }),
            transitions: Mutex::new(Vec::new()),
            reserve_transitions: Mutex::new(Vec::new()),
        };
        store.recover_all()?;
        Ok(store)
    }

    /// The validated components are appended directly, never parsed as a caller supplied path.
    pub fn stream_path(&self, stream: &StreamIdentity) -> PathBuf {
        self.root
            .join(stream.namespace.as_str())
            .join(stream.pod_uid.as_str())
            .join(stream.container.as_str())
    }

    pub fn append(&self, stream: &StreamIdentity, record: &Value) -> Result<u64, StoreError> {
        if !record.is_object() {
            return Err(StoreError::NonObjectRecord);
        }
        let _policy = self.policy_lock.lock().expect("policy lock poisoned");
        self.check_reserve()?;
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        self.enforce_retention(stream, line.len() as u64)?;
        let dir = self.create_stream_dir(stream)?;
        self.recover_directory(&dir)?;
        let hour = hour_key(self.clock.now());
        let (path, logical) = self.active_for_hour(&dir, &hour)?;
        if logical > 0 && logical.saturating_add(line.len() as u64) > self.config.max_logical_bytes
        {
            self.close_active(&path)?;
            drop(_policy);
            return self.append(stream, record);
        }
        let mut file = OpenOptions::new().append(true).open(&path)?;
        // Serialize the entire framed record before opening the append operation: this is one complete JSON-line write.
        file.write_all(&line)?;
        file.sync_data()?;
        write_logical_bytes(&sidecar(&path), logical + line.len() as u64)?;
        Ok(line.len() as u64)
    }

    pub fn recover_stream(&self, stream: &StreamIdentity) -> Result<(), StoreError> {
        let dir = self.create_stream_dir(stream)?;
        self.recover_directory(&dir)
    }

    /// Complete any transition left in the caller-prepared store before accepting appends.
    fn recover_all(&self) -> Result<(), StoreError> {
        for namespace in fs::read_dir(&self.root)? {
            let namespace = namespace?.path();
            if !namespace.is_dir() {
                continue;
            }
            for pod in fs::read_dir(namespace)? {
                let pod = pod?.path();
                if !pod.is_dir() {
                    continue;
                }
                for container in fs::read_dir(pod)? {
                    let container = container?.path();
                    if container.is_dir() {
                        self.recover_directory(&container)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Create each stream component independently so a process umask cannot
    /// leave namespace or pod directories more permissive than the contract.
    fn create_stream_dir(&self, stream: &StreamIdentity) -> Result<PathBuf, StoreError> {
        let namespace = self.root.join(stream.namespace.as_str());
        let pod = namespace.join(stream.pod_uid.as_str());
        let container = pod.join(stream.container.as_str());
        create_dir(&self.root)?;
        create_dir(&namespace)?;
        create_dir(&pod)?;
        create_dir(&container)?;
        Ok(container)
    }

    fn active_for_hour(&self, dir: &Path, hour: &str) -> Result<(PathBuf, u64), StoreError> {
        for path in active_segments(dir, None)? {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if !name.starts_with(hour) {
                self.close_active(&path)?;
            }
        }
        let mut candidates = active_segments(dir, Some(hour))?;
        candidates.sort();
        if let Some(path) = candidates.pop() {
            let logical = logical_bytes(&path)?;
            if logical < self.config.max_logical_bytes {
                return Ok((path, logical));
            }
            self.close_active(&path)?;
        }
        let sequence = next_sequence(dir, hour)?;
        let path = dir.join(format!("{hour}-{sequence:06}.jsonl.active"));
        create_file(&path)?.sync_all()?;
        write_logical_bytes(&sidecar(&path), 0)?;
        Ok((path, 0))
    }

    fn close_active(&self, active: &Path) -> Result<(), StoreError> {
        let closed = replace_suffix(active, ".active", ".closed")?;
        let active_sidecar = sidecar(active);
        let closed_sidecar = sidecar(&closed);
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(active)?
            .sync_all()?;
        fs::rename(active, &closed)?;
        if active_sidecar.exists() {
            fs::rename(active_sidecar, &closed_sidecar)?;
        }
        sync_directory(closed.parent().expect("segment has parent"))?;
        self.compress_closed(&closed)
    }

    fn compress_closed(&self, closed: &Path) -> Result<(), StoreError> {
        let gzip = replace_suffix(closed, ".closed", ".gz")?;
        let temp = PathBuf::from(format!("{}.tmp", gzip.display()));
        if gzip.exists() {
            move_sidecar(&sidecar(closed), &sidecar(&gzip))?;
            sync_directory(gzip.parent().expect("segment has parent"))?;
            fs::remove_file(closed)?;
            sync_directory(gzip.parent().expect("segment has parent"))?;
            return Ok(());
        }
        if temp.exists() {
            fs::remove_file(&temp)?;
        }
        self.compressor.gzip(closed, &temp)?;
        fs::rename(&temp, &gzip)?;
        sync_directory(gzip.parent().expect("segment has parent"))?;
        // Move accounting before deleting its source so either crash state can
        // be reconciled on restart.
        move_sidecar(&sidecar(closed), &sidecar(&gzip))?;
        sync_directory(gzip.parent().expect("segment has parent"))?;
        fs::remove_file(closed)?;
        sync_directory(gzip.parent().expect("segment has parent"))?;
        Ok(())
    }

    fn recover_directory(&self, dir: &Path) -> Result<(), StoreError> {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if name.ends_with(".jsonl.gz.tmp") {
                let gzip = PathBuf::from(name.strip_suffix(".tmp").expect("suffix checked"));
                let gzip = dir.join(gzip);
                let closed = replace_suffix(&gzip, ".gz", ".closed")?;
                if closed.exists() {
                    fs::remove_file(&path)?;
                } else if valid_gzip(&path) {
                    fs::rename(&path, gzip)?;
                } else {
                    fs::remove_file(&path)?;
                }
            }
        }
        // Recover both sidecar-only interruption windows before compression.
        for closed in closed_segments(dir)? {
            let active = replace_suffix(&closed, ".closed", ".active")?;
            move_sidecar(&sidecar(&active), &sidecar(&closed))?;
        }
        for gzip in gzip_segments(dir)? {
            let closed = replace_suffix(&gzip, ".gz", ".closed")?;
            move_sidecar(&sidecar(&closed), &sidecar(&gzip))?;
        }
        for active in active_segments(dir, None)? {
            let complete = complete_prefix_len(&active)?;
            let actual = fs::metadata(&active)?.len();
            if complete != actual {
                OpenOptions::new()
                    .write(true)
                    .open(&active)?
                    .set_len(complete)?;
            }
            write_logical_bytes(&sidecar(&active), complete)?;
        }
        for closed in closed_segments(dir)? {
            self.compress_closed(&closed)?;
        }
        Ok(())
    }
    pub fn writable_state(&self) -> Result<WritableState, StoreError> {
        // Observing the state must not turn an exhausted store into an error:
        // HTTP health/status callers need the typed unwritable state.
        let _ = self.check_reserve();
        Ok(self.state.lock().expect("state lock poisoned").clone())
    }
    pub fn eviction_transitions(&self) -> Vec<EvictionTransition> {
        self.transitions
            .lock()
            .expect("transition lock poisoned")
            .clone()
    }
    pub fn reserve_transitions(&self) -> Vec<ReserveTransition> {
        self.reserve_transitions
            .lock()
            .expect("reserve transition lock poisoned")
            .clone()
    }
    fn check_reserve(&self) -> Result<(), StoreError> {
        let total = self.capacity.total_bytes(&self.root)?;
        let available = self.capacity.available_bytes(&self.root)?;
        let required = self
            .config
            .minimum_reserve_bytes
            .max(total.saturating_mul(self.config.reserve_percent as u64) / 100);
        let writable = available >= required;
        let mut state = self.state.lock().expect("state lock poisoned");
        let next = WritableState {
            writable,
            required_reserve_bytes: required,
            available_bytes: available,
        };
        if state.writable != writable {
            self.transitions
                .lock()
                .expect("transition lock poisoned")
                .push(EvictionTransition {
                    reason: EvictionReason::Reserve,
                    logical_bytes: 0,
                });
            self.reserve_transitions
                .lock()
                .expect("reserve transition lock poisoned")
                .push(ReserveTransition {
                    state: next.clone(),
                });
        }
        *state = next;
        if writable {
            Ok(())
        } else {
            Err(StoreError::ReserveExhausted)
        }
    }
    fn enforce_retention(&self, stream: &StreamIdentity, added: u64) -> Result<(), StoreError> {
        let cutoff = hour_key(self.clock.now() - self.config.max_age);
        self.enforce_age(&cutoff)?;
        self.enforce_quota(
            stream,
            added,
            self.config.max_stream_logical_bytes,
            EvictionReason::StreamQuota,
        )?;
        self.enforce_quota(
            stream,
            added,
            self.config.max_global_logical_bytes,
            EvictionReason::GlobalQuota,
        )
    }
    /// Age expiry may rotate an active segment, but must never rewrite it.
    /// Re-scan after every transition because closing changes the segment's
    /// eligible eviction state.
    fn enforce_age(&self, cutoff: &str) -> Result<(), StoreError> {
        loop {
            let segments = self.all_segments()?;
            if let Some(closed) = segments
                .iter()
                .filter(|segment| !segment.active && segment.key.as_str() < cutoff)
                .min_by_key(|segment| (&segment.key, &segment.path))
            {
                self.evict(closed.clone(), EvictionReason::Age)?;
                continue;
            }
            if let Some(active) = segments
                .iter()
                .filter(|segment| segment.active && segment.key.as_str() < cutoff)
                .min_by_key(|segment| (&segment.key, &segment.path))
            {
                self.close_active(&active.path)?;
                continue;
            }
            return Ok(());
        }
    }
    fn enforce_quota(
        &self,
        stream: &StreamIdentity,
        added: u64,
        limit: u64,
        reason: EvictionReason,
    ) -> Result<(), StoreError> {
        loop {
            let segments = self.all_segments()?;
            let stream_dir = self.stream_path(stream);
            let total: u64 = if reason == EvictionReason::StreamQuota {
                segments
                    .iter()
                    .filter(|s| s.dir == stream_dir)
                    .map(|s| s.logical_bytes)
                    .sum()
            } else {
                segments.iter().map(|s| s.logical_bytes).sum()
            };
            if total.saturating_add(added) <= limit {
                return Ok(());
            }
            let candidate = segments
                .iter()
                .filter(|s| {
                    !s.active && (reason != EvictionReason::StreamQuota || s.dir == stream_dir)
                })
                .min_by_key(|s| (&s.key, &s.path));
            if let Some(segment) = candidate {
                self.evict(segment.clone(), reason)?;
                continue;
            }
            // Closed segments always win. Rotate an active segment only when
            // there is no eligible closed segment left to evict.
            let active = segments
                .iter()
                .filter(|s| {
                    s.active && (reason != EvictionReason::StreamQuota || s.dir == stream_dir)
                })
                .min_by_key(|s| (&s.key, &s.path));
            if let Some(active) = active {
                self.close_active(&active.path)?;
            } else {
                return Err(StoreError::QuotaCannotBeSatisfied);
            }
        }
    }
    fn evict(&self, segment: Segment, reason: EvictionReason) -> Result<(), StoreError> {
        fs::remove_file(&segment.path)?;
        let marker = sidecar(&segment.path);
        if marker.exists() {
            fs::remove_file(marker)?;
        }
        sync_directory(&segment.dir)?;
        self.transitions
            .lock()
            .expect("transition lock poisoned")
            .push(EvictionTransition {
                reason,
                logical_bytes: segment.logical_bytes,
            });
        Ok(())
    }
    fn all_segments(&self) -> Result<Vec<Segment>, StoreError> {
        let mut result = Vec::new();
        for namespace in fs::read_dir(&self.root)? {
            let namespace = namespace?.path();
            if !namespace.is_dir() {
                continue;
            }
            for pod in fs::read_dir(namespace)? {
                let pod = pod?.path();
                if !pod.is_dir() {
                    continue;
                }
                for dir in fs::read_dir(pod)? {
                    let dir = dir?.path();
                    if !dir.is_dir() {
                        continue;
                    }
                    for path in gzip_segments(&dir)?
                        .into_iter()
                        .chain(active_segments(&dir, None)?)
                    {
                        let key = path
                            .file_name()
                            .and_then(|v| v.to_str())
                            .unwrap_or_default()
                            .split('-')
                            .next()
                            .unwrap_or_default()
                            .to_owned();
                        result.push(Segment {
                            logical_bytes: logical_bytes(&path)?,
                            active: path.to_string_lossy().ends_with(".active"),
                            path,
                            key,
                            dir: dir.clone(),
                        });
                    }
                }
            }
        }
        Ok(result)
    }
}

fn create_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(DIR_MODE))?;
    }
    Ok(())
}
fn create_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE))?;
    }
    Ok(file)
}
fn write_logical_bytes(path: &Path, bytes: u64) -> io::Result<()> {
    let temporary = PathBuf::from(format!("{}.tmp", path.display()));
    let mut file = create_file(&temporary)?;
    file.write_all(bytes.to_string().as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(())
}
/// Preserve a completed destination accounting marker if recovery observes an
/// interrupted duplicate source marker as well.
fn move_sidecar(source: &Path, destination: &Path) -> io::Result<()> {
    if !source.exists() {
        return Ok(());
    }
    if destination.exists() {
        fs::remove_file(source)?;
    } else {
        fs::rename(source, destination)?;
    }
    Ok(())
}
fn logical_bytes(path: &Path) -> io::Result<u64> {
    let marker = sidecar(path);
    match fs::read_to_string(marker) {
        Ok(value) => Ok(value.trim().parse().unwrap_or(0)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => complete_prefix_len(path),
        Err(error) => Err(error),
    }
}
fn sidecar(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.bytes", path.display()))
}
fn replace_suffix(path: &Path, suffix: &str, replacement: &str) -> Result<PathBuf, StoreError> {
    let value = path.to_string_lossy();
    let Some(stem) = value.strip_suffix(suffix) else {
        return Err(StoreError::InvalidSegmentName(value.into_owned()));
    };
    Ok(PathBuf::from(format!("{stem}{replacement}")))
}
fn hour_key(time: OffsetDateTime) -> String {
    let time = time.to_offset(UtcOffset::UTC);
    format!(
        "{:04}{:02}{:02}T{:02}0000Z",
        time.year(),
        u8::from(time.month()),
        time.day(),
        time.hour()
    )
}
fn active_segments(dir: &Path, hour: Option<&str>) -> Result<Vec<PathBuf>, StoreError> {
    segments(dir, ".jsonl.active", hour)
}
fn closed_segments(dir: &Path) -> Result<Vec<PathBuf>, StoreError> {
    segments(dir, ".jsonl.closed", None)
}
fn gzip_segments(dir: &Path) -> Result<Vec<PathBuf>, StoreError> {
    segments(dir, ".jsonl.gz", None)
}
fn segments(dir: &Path, ending: &str, hour: Option<&str>) -> Result<Vec<PathBuf>, StoreError> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if name.ends_with(ending) && hour.is_none_or(|hour| name.starts_with(hour)) {
            paths.push(path);
        }
    }
    Ok(paths)
}
fn next_sequence(dir: &Path, hour: &str) -> Result<u32, StoreError> {
    let mut maximum = None;
    for path in active_segments(dir, Some(hour))?
        .into_iter()
        .chain(closed_segments(dir)?)
        .chain(gzip_segments(dir)?)
    {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if let Some(sequence) = name
            .strip_prefix(&format!("{hour}-"))
            .and_then(|rest| rest.split('.').next())
            .and_then(|value| value.parse::<u32>().ok())
        {
            maximum = Some(maximum.map_or(sequence, |old: u32| old.max(sequence)));
        }
    }
    Ok(maximum.map_or(0, |sequence| sequence + 1))
}
fn complete_prefix_len(path: &Path) -> io::Result<u64> {
    let bytes = fs::read(path)?;
    Ok(bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |offset| offset as u64 + 1))
}
fn valid_gzip(path: &Path) -> bool {
    let mut output = Vec::new();
    match File::open(path) {
        Ok(file) => GzDecoder::new(file).read_to_end(&mut output).is_ok(),
        Err(_) => false,
    }
}
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

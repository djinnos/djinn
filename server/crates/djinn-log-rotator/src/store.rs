use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde_json::Value;
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset};

use crate::StreamIdentity;

pub const DEFAULT_MAX_LOGICAL_BYTES: u64 = 128 * 1024 * 1024;
const DIR_MODE: u32 = 0o750;
const FILE_MODE: u32 = 0o640;

pub trait Clock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
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
}
impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            max_logical_bytes: DEFAULT_MAX_LOGICAL_BYTES,
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
}

pub struct LogStore<C: Clock = SystemClock, Z: Compressor = GzipCompressor> {
    root: PathBuf,
    config: StoreConfig,
    clock: Arc<C>,
    compressor: Arc<Z>,
}
impl LogStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        Self::with_parts(root, StoreConfig::default(), SystemClock, GzipCompressor)
    }
}
impl<C: Clock, Z: Compressor> LogStore<C, Z> {
    pub fn with_parts(
        root: impl Into<PathBuf>,
        config: StoreConfig,
        clock: C,
        compressor: Z,
    ) -> Result<Self, StoreError> {
        let root = root.into();
        create_dir(&root)?;
        let store = Self {
            root,
            config,
            clock: Arc::new(clock),
            compressor: Arc::new(compressor),
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
        let dir = self.stream_path(stream);
        create_dir(&dir)?;
        self.recover_directory(&dir)?;
        let hour = hour_key(self.clock.now());
        let (path, logical) = self.active_for_hour(&dir, &hour)?;
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        if logical > 0 && logical.saturating_add(line.len() as u64) > self.config.max_logical_bytes
        {
            self.close_active(&path)?;
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
        let dir = self.stream_path(stream);
        create_dir(&dir)?;
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
            fs::remove_file(closed)?;
            let closed_sidecar = sidecar(closed);
            if closed_sidecar.exists() {
                fs::rename(closed_sidecar, sidecar(&gzip))?;
            }
            return Ok(());
        }
        if temp.exists() {
            fs::remove_file(&temp)?;
        }
        self.compressor.gzip(closed, &temp)?;
        fs::rename(&temp, &gzip)?;
        sync_directory(gzip.parent().expect("segment has parent"))?;
        fs::remove_file(closed)?;
        let closed_sidecar = sidecar(closed);
        if closed_sidecar.exists() {
            fs::rename(closed_sidecar, sidecar(&gzip))?;
        }
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
        .chain(closed_segments(dir)?.into_iter())
        .chain(gzip_segments(dir)?.into_iter())
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

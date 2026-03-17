use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::{Mutex, RwLock};

type SessionId = String;
type NormalizedPath = String;
type ReadRecord = (SystemTime, Option<SystemTime>);
type SessionFileTimes = HashMap<NormalizedPath, ReadRecord>;
type FileTimeMap = HashMap<SessionId, SessionFileTimes>;

#[derive(Default)]
pub struct FileTime {
    // session_id -> (normalized_path -> (read_at, mtime_at_read))
    inner: RwLock<FileTimeMap>,
    // Per-file write locks to serialize concurrent writes to the same file
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl FileTime {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn read(&self, session_id: &str, path: &Path) -> Result<(), String> {
        let normalized = normalize(path);
        let now = SystemTime::now();
        let mtime = file_mtime(path)?;
        let mut guard = self.inner.write().await;
        let by_path = guard.entry(session_id.to_string()).or_default();
        by_path.insert(normalized, (now, mtime));
        Ok(())
    }

    pub async fn get(&self, session_id: &str, path: &Path) -> Option<SystemTime> {
        let normalized = normalize(path);
        let guard = self.inner.read().await;
        guard
            .get(session_id)
            .and_then(|m| m.get(&normalized).map(|(read_at, _)| *read_at))
    }

    pub async fn assert(&self, session_id: &str, path: &Path) -> Result<(), String> {
        let normalized = normalize(path);
        let (read_at, read_mtime) = {
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
        if current_mtime != read_mtime {
            return Err(format!(
                "file was modified since last read in this session: {} (last_read={:?})",
                path.display(),
                read_at
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
            Arc::clone(map.entry(canonical).or_insert_with(|| Arc::new(Mutex::new(()))))
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

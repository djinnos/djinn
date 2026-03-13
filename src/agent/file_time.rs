use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use tokio::sync::RwLock;

type SessionId = String;
type NormalizedPath = String;
type ReadRecord = (SystemTime, Option<SystemTime>);
type SessionFileTimes = HashMap<NormalizedPath, ReadRecord>;
type FileTimeMap = HashMap<SessionId, SessionFileTimes>;

#[derive(Default)]
pub struct FileTime {
    // session_id -> (normalized_path -> (read_at, mtime_at_read))
    inner: RwLock<FileTimeMap>,
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

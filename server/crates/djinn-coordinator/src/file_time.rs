//! Compatibility shim: `FileTime` utility.
//!
//! Per-session file-mtime tracker used by the coordinator's maintenance
//! context construction.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct FileTime {
    inner: Arc<RwLock<HashMap<(String, String), SystemTime>>>,
}

impl FileTime {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn read(&self, session_id: &str, path: &Path) -> Result<(), String> {
        let key = (session_id.to_owned(), path.to_string_lossy().to_string());
        let mtime = tokio::fs::metadata(path)
            .await
            .and_then(|m| m.modified())
            .map_err(|e| e.to_string())?;
        self.inner.write().await.insert(key, mtime);
        Ok(())
    }

    pub async fn invalidate(&self, session_id: &str, path: &Path) {
        let key = (session_id.to_owned(), path.to_string_lossy().to_string());
        self.inner.write().await.remove(&key);
    }

    pub async fn get(&self, session_id: &str, path: &Path) -> Option<SystemTime> {
        let key = (session_id.to_owned(), path.to_string_lossy().to_string());
        self.inner.read().await.get(&key).copied()
    }

    pub async fn assert(&self, session_id: &str, path: &Path) -> Result<(), String> {
        let stored = self.get(session_id, path).await;
        let current = tokio::fs::metadata(path)
            .await
            .and_then(|m| m.modified())
            .map_err(|e| e.to_string())?;

        match stored {
            Some(t) if t == current => Ok(()),
            Some(_) => Err(format!(
                "file {} was modified since last read",
                path.display()
            )),
            None => Err(format!("no mtime recorded for {}", path.display())),
        }
    }
}

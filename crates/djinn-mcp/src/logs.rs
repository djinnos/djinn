/// Log file utilities for the system_logs MCP tool.
use std::path::PathBuf;

const LOG_FILE_PREFIX: &str = "djinn.log";

pub fn logs_dir() -> PathBuf {
    dirs::home_dir()
        .expect("cannot determine home directory")
        .join(".djinn")
        .join("logs")
}

/// Returns the path to the most recent djinn log file, or `None` if no log
/// file exists yet.
pub fn latest_log_file_path() -> Option<PathBuf> {
    let dir = logs_dir();
    let mut entries = std::fs::read_dir(&dir).ok()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    while let Some(Ok(entry)) = entries.next() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(LOG_FILE_PREFIX) {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
        {
            if best.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
                best = Some((modified, entry.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

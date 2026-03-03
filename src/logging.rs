use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

const LOG_FILE_PREFIX: &str = "djinn.log";
const LOG_RETENTION_DAYS: u64 = 7;

pub fn logs_dir() -> PathBuf {
    dirs::home_dir()
        .expect("cannot determine home directory")
        .join(".djinn")
        .join("logs")
}

pub fn setup_log_dir_and_retention() {
    let dir = logs_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("failed to create log directory {}: {e}", dir.display());
        return;
    }

    if let Err(e) = prune_old_logs(&dir) {
        eprintln!("failed to prune old logs in {}: {e}", dir.display());
    }
}

pub fn latest_log_file_path() -> Option<PathBuf> {
    let dir = logs_dir();
    let entries = fs::read_dir(dir).ok()?;

    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let file_name = path.file_name()?.to_str()?;
            if !file_name.starts_with(LOG_FILE_PREFIX) {
                return None;
            }

            let modified = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);

            Some((modified, path))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

pub fn file_prefix() -> &'static str {
    LOG_FILE_PREFIX
}

fn prune_old_logs(dir: &std::path::Path) -> std::io::Result<()> {
    let now = SystemTime::now();
    let keep_for = Duration::from_secs(LOG_RETENTION_DAYS * 24 * 60 * 60);

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !file_name.starts_with(LOG_FILE_PREFIX) {
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };

        let Ok(age) = now.duration_since(modified) else {
            continue;
        };

        if age > keep_for {
            let _ = fs::remove_file(path);
        }
    }

    Ok(())
}

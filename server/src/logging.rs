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

/// Errors here are emitted via `eprintln!` deliberately: this function runs
/// *before* the tracing subscriber is initialised in `init_logging`, so
/// `tracing::error!` would silently drop the message. stderr is the only
/// reliable channel for pre-subscriber diagnostics.
#[allow(clippy::print_stderr)]
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

pub fn file_prefix() -> &'static str {
    LOG_FILE_PREFIX
}

#[allow(clippy::disallowed_methods)] // scoped: direct wall-clock read; migration tracked by lint-ratchet task 70y0 (Clock abstraction already lands in 8bcj/m5g4)
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

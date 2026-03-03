use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub pid: u32,
    pub port: u16,
    pub started_at: String,
}

pub struct DaemonLock {
    path: PathBuf,
}

impl DaemonLock {
    pub fn release(&self) {
        if let Err(e) = fs::remove_file(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %e, path = %self.path.display(), "failed to remove daemon lockfile");
        }
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        self.release();
    }
}

pub fn daemon_file_path() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| "failed to resolve home directory".to_string())?;
    Ok(home.join(".djinn").join("daemon.json"))
}

pub fn acquire(port: u16) -> Result<DaemonLock, String> {
    let path = daemon_file_path()?;
    let current_pid = std::process::id();

    if path.exists() {
        match read_daemon_info(&path) {
            Ok(Some(existing)) => {
                if existing.pid != current_pid && pid_is_alive(existing.pid) {
                    return Err(format!(
                        "another djinn-server is already running (pid={}, port={})",
                        existing.pid, existing.port
                    ));
                }
                tracing::warn!(
                    pid = existing.pid,
                    path = %path.display(),
                    "stale daemon lockfile detected, reclaiming"
                );
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "failed to parse daemon lockfile, reclaiming");
            }
        }
    }

    let started_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|e| format!("format startup timestamp: {e}"))?;
    let info = DaemonInfo {
        pid: current_pid,
        port,
        started_at,
    };
    write_daemon_info(&path, &info)?;

    Ok(DaemonLock { path })
}

fn read_daemon_info(path: &Path) -> Result<Option<DaemonInfo>, String> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let info = serde_json::from_str::<DaemonInfo>(&raw)
        .map_err(|e| format!("parse {}: {e}", path.display()))?;
    Ok(Some(info))
}

fn write_daemon_info(path: &Path, info: &DaemonInfo) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }

    let tmp_path = path.with_extension("json.tmp");
    let payload =
        serde_json::to_vec_pretty(info).map_err(|e| format!("serialize daemon info: {e}"))?;

    {
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;

            fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&tmp_path)
                .map_err(|e| format!("open {}: {e}", tmp_path.display()))?
        };

        #[cfg(not(unix))]
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_path)
            .map_err(|e| format!("open {}: {e}", tmp_path.display()))?;

        file.write_all(&payload)
            .map_err(|e| format!("write {}: {e}", tmp_path.display()))?;
        file.write_all(b"\n")
            .map_err(|e| format!("write newline {}: {e}", tmp_path.display()))?;
        file.sync_all()
            .map_err(|e| format!("sync {}: {e}", tmp_path.display()))?;
    }

    fs::rename(&tmp_path, path)
        .map_err(|e| format!("rename {} -> {}: {e}", tmp_path.display(), path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }

    Ok(())
}

fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if pid == std::process::id() {
        return true;
    }

    #[cfg(target_os = "linux")]
    {
        return Path::new(&format!("/proc/{pid}")).exists();
    }

    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_reads_daemon_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        let info = DaemonInfo {
            pid: 123,
            port: 8372,
            started_at: "2026-03-03T18:00:00Z".to_string(),
        };

        write_daemon_info(&path, &info).unwrap();
        let parsed = read_daemon_info(&path).unwrap().unwrap();

        assert_eq!(parsed.pid, 123);
        assert_eq!(parsed.port, 8372);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn reads_missing_daemon_file_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");
        let parsed = read_daemon_info(&path).unwrap();
        assert!(parsed.is_none());
    }
}

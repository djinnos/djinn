use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

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

pub fn read_daemon_info(path: &Path) -> Result<Option<DaemonInfo>, String> {
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

pub fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if pid == std::process::id() {
        return true;
    }

    #[cfg(unix)]
    {
        // POSIX kill(pid, 0) checks process existence without sending a signal.
        // Returns 0 if process exists and we can signal it.
        // Returns -1 with EPERM if process exists but we lack permission.
        // Returns -1 with ESRCH if process does not exist.
        let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if ret == 0 {
            return true;
        }
        // EPERM means the process exists but we can't signal it (different user).
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    #[cfg(not(unix))]
    {
        false
    }
}

/// Read daemon info from the default path (`~/.djinn/daemon.json`).
pub fn read_info_default() -> Option<DaemonInfo> {
    let path = daemon_file_path().ok()?;
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Ensure a daemon is running on the given port, spawning one from
/// `server_bin` if necessary. Returns the [`DaemonInfo`] of the running
/// daemon.
pub async fn ensure_running(
    port: u16,
    db_path: Option<&Path>,
    server_bin: &Path,
) -> Result<DaemonInfo, String> {
    if let Some(info) = read_info_default()
        && pid_is_alive(info.pid)
    {
        tracing::info!(pid = info.pid, port = info.port, "daemon already running");
        return Ok(info);
    }

    let child = spawn_daemon(server_bin, port, db_path)?;
    wait_for_daemon(child).await
}

fn spawn_daemon(
    server_bin: &Path,
    port: u16,
    db_path: Option<&Path>,
) -> Result<std::process::Child, String> {
    let mut cmd = std::process::Command::new(server_bin);
    cmd.arg("--port").arg(port.to_string());
    if let Some(path) = db_path {
        cmd.arg("--db-path").arg(path);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // Place the daemon in its own session so it is immune to SIGHUP/SIGINT
    // from the parent's terminal or process group.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid() is async-signal-safe (POSIX) and has no
        // preconditions beyond "caller is not already a session leader".
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    cmd.spawn()
        .map_err(|e| format!("spawn daemon process from {}: {e}", server_bin.display()))
}

async fn wait_for_daemon(mut child: std::process::Child) -> Result<DaemonInfo, String> {
    for _ in 0..40 {
        if let Some(info) = read_info_default()
            && pid_is_alive(info.pid)
        {
            tracing::info!(pid = info.pid, port = info.port, "daemon started");
            return Ok(info);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    match child.try_wait() {
        Ok(Some(status)) => Err(format!("daemon process exited early: {status}")),
        Ok(None) => Err("daemon did not become healthy in time".to_string()),
        Err(e) => Err(format!("check daemon process status: {e}")),
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

    #[test]
    fn current_process_is_alive() {
        assert!(pid_is_alive(std::process::id()));
    }

    #[test]
    fn bogus_pid_is_not_alive() {
        // PID 4_000_000 is far beyond typical PID ranges on any system.
        assert!(!pid_is_alive(4_000_000));
    }

    #[test]
    fn zero_pid_is_not_alive() {
        assert!(!pid_is_alive(0));
    }

    #[test]
    fn pid_one_is_alive() {
        // PID 1 (init/launchd) is always running on Unix.
        #[cfg(unix)]
        assert!(pid_is_alive(1));
    }

    #[test]
    fn acquire_detects_running_daemon() {
        // Simulate a lockfile written by the current process with a different
        // "daemon PID" that is actually alive (PID 1 = init/launchd).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        let info = DaemonInfo {
            pid: 1, // PID 1 is always alive on Unix
            port: 9999,
            started_at: "2026-01-01T00:00:00Z".to_string(),
        };
        write_daemon_info(&path, &info).unwrap();

        // Verify pid_is_alive correctly detects PID 1 as alive.
        #[cfg(unix)]
        assert!(pid_is_alive(1));
    }

    #[test]
    fn acquire_reclaims_stale_lockfile() {
        // Simulate a lockfile from a dead process.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        let info = DaemonInfo {
            pid: 4_000_000, // dead PID
            port: 8372,
            started_at: "2026-01-01T00:00:00Z".to_string(),
        };
        write_daemon_info(&path, &info).unwrap();

        // pid_is_alive should return false for the dead PID.
        assert!(!pid_is_alive(4_000_000));

        // Read it back and verify it was the stale one.
        let parsed = read_daemon_info(&path).unwrap().unwrap();
        assert_eq!(parsed.pid, 4_000_000);
    }
}

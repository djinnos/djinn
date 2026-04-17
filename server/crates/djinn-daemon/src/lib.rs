#![warn(unreachable_pub)]

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
    /// Process start time in clock ticks since system boot, read from
    /// `/proc/<pid>/stat` field 22 on Linux. Used as a tiebreaker when the
    /// PID is alive but potentially recycled — e.g. after a container
    /// restart where the old lockfile's PID happens to be reused by a
    /// tokio worker thread in the new container. `None` on non-Linux or
    /// when the previous writer predates this field (parsed via
    /// `serde(default)` for backward compatibility with old lockfiles).
    #[serde(default)]
    pub pid_starttime: Option<u64>,
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
                if existing.pid != current_pid && process_matches_info(&existing) {
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
        pid_starttime: read_pid_starttime(current_pid),
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

/// Read field 22 (starttime, clock ticks since system boot) from
/// `/proc/<pid>/stat` on Linux. Returns `None` on other platforms or if the
/// PID has no `/proc` entry. Pairs with [`process_matches_info`] to detect
/// PID recycling across container restarts.
pub fn read_pid_starttime(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        // `/proc/<pid>/stat` layout: `pid (comm) state ppid ...` where
        // `comm` may itself contain spaces or parens, so we can't naive-split.
        // Convention: take the substring after the *last* ')' and split that.
        let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_comm = raw.rsplit_once(')').map(|(_, rest)| rest)?;
        // `after_comm` now starts with " state ppid ...". starttime is at
        // index 22 overall; index 20 after trimming the pid and comm fields.
        after_comm.split_whitespace().nth(19)?.parse::<u64>().ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Return `true` iff the process recorded in `info` is actually still the
/// one holding the daemon slot — i.e. its PID is alive AND its start time
/// matches the one stored in the lockfile.
///
/// Without the start-time check, a stale lockfile whose PID happened to
/// be recycled (common on container restart: tokio worker threads occupy
/// the recently-freed PID range) would look alive and trigger a false
/// "another djinn-server is already running" error. With it, a recycled
/// PID is detected as a mismatch and the lockfile is correctly reclaimed.
pub fn process_matches_info(info: &DaemonInfo) -> bool {
    if !pid_is_alive(info.pid) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        match (read_pid_starttime(info.pid), info.pid_starttime) {
            (Some(current), Some(stored)) => current == stored,
            // Lockfiles predating the pid_starttime field can't be
            // verified — treat as stale so the new writer reclaims.
            _ => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // No cheap start-time source on non-Linux; pid-alive is the best
        // we have. The container-restart-PID-recycling scenario this
        // guards against is Linux-specific anyway.
        true
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
        && process_matches_info(&info)
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
        .stderr(Stdio::piped());

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
            && process_matches_info(&info)
        {
            tracing::info!(pid = info.pid, port = info.port, "daemon started");
            return Ok(info);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Capture stderr for diagnostics when the daemon fails to start.
    let stderr_output = child
        .stderr
        .take()
        .and_then(|mut stderr| {
            use std::io::Read;
            let mut buf = String::new();
            stderr.read_to_string(&mut buf).ok().map(|_| buf)
        })
        .unwrap_or_default();

    let stderr_hint = if stderr_output.is_empty() {
        String::new()
    } else {
        format!("\nstderr: {}", stderr_output.trim())
    };

    match child.try_wait() {
        Ok(Some(status)) => Err(format!(
            "daemon process exited early: {status}{stderr_hint}"
        )),
        Ok(None) => Err(format!(
            "daemon did not become healthy in time{stderr_hint}"
        )),
        Err(e) => Err(format!("check daemon process status: {e}{stderr_hint}")),
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
            pid_starttime: Some(1234567),
        };

        write_daemon_info(&path, &info).unwrap();
        let parsed = read_daemon_info(&path).unwrap().unwrap();

        assert_eq!(parsed.pid, 123);
        assert_eq!(parsed.port, 8372);
        assert_eq!(parsed.pid_starttime, Some(1234567));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn reads_legacy_daemon_file_without_pid_starttime() {
        // Lockfiles written before the pid_starttime field existed should
        // still parse — the field is `#[serde(default)]`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        fs::write(
            &path,
            r#"{"pid":123,"port":8372,"started_at":"2026-03-03T18:00:00Z"}"#,
        )
        .unwrap();
        let parsed = read_daemon_info(&path).unwrap().unwrap();
        assert_eq!(parsed.pid, 123);
        assert_eq!(parsed.pid_starttime, None);
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
    fn process_matches_info_accepts_live_process_with_matching_starttime() {
        // PID 1 is always alive on Unix; read its real starttime so the
        // recorded value agrees with what the kernel reports right now.
        #[cfg(target_os = "linux")]
        {
            let st = read_pid_starttime(1).expect("PID 1 has /proc/1/stat");
            let info = DaemonInfo {
                pid: 1,
                port: 9999,
                started_at: "2026-01-01T00:00:00Z".to_string(),
                pid_starttime: Some(st),
            };
            assert!(process_matches_info(&info));
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            // No /proc on macOS/BSD — fall back to pure PID-alive check.
            let info = DaemonInfo {
                pid: 1,
                port: 9999,
                started_at: "2026-01-01T00:00:00Z".to_string(),
                pid_starttime: None,
            };
            assert!(process_matches_info(&info));
        }
    }

    #[test]
    fn process_matches_info_rejects_recycled_pid_on_linux() {
        // Regression for the container-restart bug: the previous
        // container's djinn-server wrote pid=8 into the lockfile; in the
        // new container PID 8 is a tokio worker thread. pid_is_alive(8)
        // returns true, so without the start-time check the lockfile
        // looked live and the server crash-looped.
        //
        // Simulate that by claiming PID 1 with a starttime that definitely
        // doesn't match PID 1's actual starttime.
        #[cfg(target_os = "linux")]
        {
            let info = DaemonInfo {
                pid: 1,
                port: 9999,
                started_at: "2026-01-01T00:00:00Z".to_string(),
                pid_starttime: Some(u64::MAX), // impossible starttime
            };
            assert!(!process_matches_info(&info));
        }
    }

    #[test]
    fn process_matches_info_rejects_legacy_lockfile_on_linux() {
        // Old lockfiles (pre-pid_starttime) parse with None. On Linux we
        // treat them as stale so the new writer reclaims, rather than
        // trusting a pure PID check that is known-broken across
        // container restarts.
        #[cfg(target_os = "linux")]
        {
            let info = DaemonInfo {
                pid: 1,
                port: 9999,
                started_at: "2026-01-01T00:00:00Z".to_string(),
                pid_starttime: None,
            };
            assert!(!process_matches_info(&info));
        }
    }

    #[test]
    fn process_matches_info_rejects_dead_pid() {
        let info = DaemonInfo {
            pid: 4_000_000,
            port: 8372,
            started_at: "2026-01-01T00:00:00Z".to_string(),
            pid_starttime: Some(1234567),
        };
        assert!(!process_matches_info(&info));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn read_pid_starttime_for_self_is_stable() {
        let a = read_pid_starttime(std::process::id()).expect("/proc/self/stat exists");
        let b = read_pid_starttime(std::process::id()).expect("/proc/self/stat exists");
        assert_eq!(a, b);
        assert!(a > 0);
    }
}

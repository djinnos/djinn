//! Per-session cgroup memory limits (Linux only, requires systemd Delegate=yes).
//!
//! When djinn runs as a systemd service with `Delegate=yes`, it can create
//! child cgroups under its own cgroup slice to enforce per-session memory limits.
//!
//! All operations are fallible and non-fatal. If cgroup delegation is not
//! available (not running under systemd, no delegation, non-Linux), everything
//! returns `None`/`Ok` gracefully.

#[cfg(target_os = "linux")]
mod inner {
    use std::fs;
    use std::path::PathBuf;

    use crate::resource_monitor::MemoryStatus;

    /// A per-session cgroup with a memory limit.
    ///
    /// On drop, remaining processes are killed and the cgroup directory is removed.
    pub struct SessionCgroup {
        path: PathBuf,
    }

    impl SessionCgroup {
        /// Create a new cgroup for a session with the given memory limit.
        ///
        /// Returns `None` if cgroup delegation is not available or creation fails.
        pub fn create(session_id: &str, memory_limit_bytes: u64) -> Option<Self> {
            let daemon_cgroup = daemon_cgroup_dir()?;

            let session_dir = daemon_cgroup.join(format!("djinn-session-{session_id}"));
            if fs::create_dir(&session_dir).ok().is_none() {
                tracing::debug!(
                    path = %session_dir.display(),
                    "failed to create session cgroup directory"
                );
                return None;
            }

            // memory.max — hard limit, OOM-killed if exceeded.
            if let Err(e) = fs::write(
                session_dir.join("memory.max"),
                memory_limit_bytes.to_string(),
            ) {
                tracing::debug!(
                    error = %e,
                    "failed to write memory.max, removing cgroup dir"
                );
                let _ = fs::remove_dir(&session_dir);
                return None;
            }

            // memory.high — soft limit at 80%, causes kernel reclaim backpressure.
            let memory_high = memory_limit_bytes * 80 / 100;
            if let Err(e) = fs::write(session_dir.join("memory.high"), memory_high.to_string()) {
                tracing::debug!(error = %e, "failed to write memory.high (non-fatal)");
            }

            tracing::debug!(
                path = %session_dir.display(),
                memory_limit_bytes,
                "created session cgroup"
            );

            Some(Self { path: session_dir })
        }

        /// Move a process into this cgroup.
        pub fn add_pid(&self, pid: u32) -> std::io::Result<()> {
            fs::write(self.path.join("cgroup.procs"), pid.to_string())
        }

        /// Path to this cgroup directory.
        pub fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for SessionCgroup {
        fn drop(&mut self) {
            // Kill any remaining processes before removing the cgroup directory.
            if let Ok(procs) = fs::read_to_string(self.path.join("cgroup.procs")) {
                for line in procs.lines() {
                    if let Ok(pid) = line.trim().parse::<i32>()
                        && pid > 0
                    {
                        unsafe {
                            libc::kill(pid, libc::SIGKILL);
                        }
                    }
                }
            }

            // Give the kernel a moment to clean up after SIGKILL before rmdir.
            // The cgroup directory can only be removed when it has no live processes.
            // In practice the kill is near-instant, but we make a few attempts.
            for _ in 0..5 {
                match fs::remove_dir(&self.path) {
                    Ok(()) => return,
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
                }
            }

            tracing::debug!(
                path = %self.path.display(),
                "could not remove session cgroup dir after retries"
            );
        }
    }

    /// Compute a per-session memory limit based on current system resources.
    ///
    /// Returns `effective_limit * 0.70 / suggested_max_sessions`, i.e. each
    /// session's fair share of the memory budget.
    pub fn per_session_memory_limit() -> Option<u64> {
        let status = MemoryStatus::read()?;
        let max_sessions = status.suggested_max_sessions().max(1) as u64;
        let budget = (status.effective_limit_bytes as f64 * 0.70) as u64;
        Some(budget / max_sessions)
    }

    /// Detect if cgroup delegation is available by checking whether we can
    /// create subdirectories under our own cgroup path.
    pub fn is_delegation_available() -> bool {
        daemon_cgroup_dir().is_some()
    }

    // ─── Internal helpers ──────────────────────────────────────────────────────

    /// Parse `/proc/self/cgroup` to find the current cgroup v2 path.
    ///
    /// The cgroup v2 line has the format `0::/slice/path`.
    fn read_self_cgroup_path() -> Option<String> {
        let contents = fs::read_to_string("/proc/self/cgroup").ok()?;
        parse_cgroup_v2_path(&contents)
    }

    /// Parse a cgroup v2 path from `/proc/self/cgroup` content.
    fn parse_cgroup_v2_path(contents: &str) -> Option<String> {
        for line in contents.lines() {
            // cgroup v2 unified hierarchy line: "0::<path>"
            if let Some(path) = line.strip_prefix("0::") {
                return Some(path.to_string());
            }
        }
        None
    }

    /// Resolve the filesystem path for the daemon's cgroup directory and verify
    /// that we have write access (i.e., delegation is available).
    fn daemon_cgroup_dir() -> Option<PathBuf> {
        let cgroup_path = read_self_cgroup_path()?;
        let dir = PathBuf::from(format!("/sys/fs/cgroup{cgroup_path}"));

        // Quick writability check: can we create+remove a probe directory?
        let probe = dir.join(".djinn-probe");
        if fs::create_dir(&probe).is_ok() {
            let _ = fs::remove_dir(&probe);
            Some(dir)
        } else {
            tracing::debug!(
                path = %dir.display(),
                "cgroup delegation not available (cannot create subdirectory)"
            );
            None
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::resource_monitor::MemoryStatus;

        #[test]
        fn parse_cgroup_v2_path_typical() {
            let input = "0::/user.slice/user-1000.slice/session-2.scope\n";
            assert_eq!(
                parse_cgroup_v2_path(input),
                Some("/user.slice/user-1000.slice/session-2.scope".to_string())
            );
        }

        #[test]
        fn parse_cgroup_v2_path_systemd_service() {
            let input = "0::/system.slice/djinn.service\n";
            assert_eq!(
                parse_cgroup_v2_path(input),
                Some("/system.slice/djinn.service".to_string())
            );
        }

        #[test]
        fn parse_cgroup_v2_path_root() {
            let input = "0::/\n";
            assert_eq!(parse_cgroup_v2_path(input), Some("/".to_string()));
        }

        #[test]
        fn parse_cgroup_v2_path_with_v1_lines() {
            // Some systems still show v1 lines alongside the v2 line.
            let input = "\
12:memory:/user.slice
11:cpuset:/
0::/user.slice/user-1000.slice/session-5.scope
";
            assert_eq!(
                parse_cgroup_v2_path(input),
                Some("/user.slice/user-1000.slice/session-5.scope".to_string())
            );
        }

        #[test]
        fn parse_cgroup_v2_path_missing() {
            let input = "12:memory:/user.slice\n11:cpuset:/\n";
            assert_eq!(parse_cgroup_v2_path(input), None);
        }

        #[test]
        fn parse_cgroup_v2_path_empty() {
            assert_eq!(parse_cgroup_v2_path(""), None);
        }

        #[test]
        fn per_session_limit_64gb() {
            // Manually verify the math: 64 GiB * 0.70 = 44.8 GiB budget,
            // suggested_max_sessions for 64 GiB = 44,
            // per_session = 44.8 GiB / 44 ~= 1.018 GiB
            let status = MemoryStatus {
                total_bytes: 64 * 1024 * 1024 * 1024,
                available_bytes: 50 * 1024 * 1024 * 1024,
                effective_limit_bytes: 64 * 1024 * 1024 * 1024,
                psi_some_avg10: 0.0,
                psi_full_avg10: 0.0,
            };
            let max_sessions = status.suggested_max_sessions() as u64; // 44
            let budget = (status.effective_limit_bytes as f64 * 0.70) as u64;
            let expected = budget / max_sessions;

            // ~1.018 GiB per session
            assert!(expected > 1024 * 1024 * 1024); // > 1 GiB
            assert!(expected < 2 * 1024 * 1024 * 1024); // < 2 GiB
        }

        #[test]
        fn per_session_limit_4gb_container() {
            let status = MemoryStatus {
                total_bytes: 64 * 1024 * 1024 * 1024,
                available_bytes: 3 * 1024 * 1024 * 1024,
                effective_limit_bytes: 4 * 1024 * 1024 * 1024,
                psi_some_avg10: 0.0,
                psi_full_avg10: 0.0,
            };
            let max_sessions = status.suggested_max_sessions() as u64; // 2
            let budget = (status.effective_limit_bytes as f64 * 0.70) as u64;
            let expected = budget / max_sessions;

            // 4 GiB * 0.70 / 2 = 1.4 GiB
            assert!(expected > 1024 * 1024 * 1024);
            assert!(expected < 2 * 1024 * 1024 * 1024);
        }
    }
}

#[cfg(target_os = "linux")]
pub use inner::*;

// ─── Non-Linux stubs ────────────────────────────────────────────────────────

/// Stub for non-Linux platforms. Always returns `None`.
#[cfg(not(target_os = "linux"))]
pub fn per_session_memory_limit() -> Option<u64> {
    None
}

/// Stub for non-Linux platforms. Always returns `false`.
#[cfg(not(target_os = "linux"))]
pub fn is_delegation_available() -> bool {
    false
}

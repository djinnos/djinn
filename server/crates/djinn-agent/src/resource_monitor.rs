//! System resource monitoring for dispatch throttling.
//!
//! Reads `/proc/meminfo` for available memory and `/proc/pressure/memory`
//! for PSI (Pressure Stall Information) to detect memory pressure before
//! the OOM killer fires.

/// Snapshot of current system memory status.
#[derive(Debug, Clone)]
pub struct MemoryStatus {
    /// Total physical memory in bytes.
    pub total_bytes: u64,
    /// Available memory in bytes (kernel estimate of reclaimable + free).
    pub available_bytes: u64,
    /// Effective limit: the lower of physical RAM and cgroup limit.
    pub effective_limit_bytes: u64,
    /// PSI "some" avg10 — percentage of time at least one task stalled on memory.
    pub psi_some_avg10: f64,
    /// PSI "full" avg10 — percentage of time ALL tasks stalled on memory.
    pub psi_full_avg10: f64,
}

impl MemoryStatus {
    /// Read current memory status from `/proc`. Returns `None` on non-Linux
    /// or if the required files cannot be read.
    #[cfg(target_os = "linux")]
    pub fn read() -> Option<Self> {
        let (total, available) = parse_meminfo()?;
        let cgroup_limit = read_cgroup_limit();
        let effective_limit = match cgroup_limit {
            Some(limit) if limit < total => limit,
            _ => total,
        };
        let (psi_some, psi_full) = parse_psi().unwrap_or((0.0, 0.0));

        Some(Self {
            total_bytes: total,
            available_bytes: available,
            effective_limit_bytes: effective_limit,
            psi_some_avg10: psi_some,
            psi_full_avg10: psi_full,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn read() -> Option<Self> {
        None
    }

    /// Suggested max concurrent sessions based on available memory.
    /// Assumes ~1 GiB per session (shared rust-analyzer + agent overhead).
    pub fn suggested_max_sessions(&self) -> u32 {
        let budget = (self.effective_limit_bytes as f64 * 0.70) as u64;
        let per_session: u64 = 1024 * 1024 * 1024; // 1 GiB estimate
        (budget / per_session).max(1) as u32
    }

    /// Whether memory pressure suggests we should pause new dispatches.
    pub fn should_throttle(&self) -> bool {
        self.psi_some_avg10 > 15.0
    }

    /// Whether memory pressure is critical (all tasks stalled).
    pub fn is_critical(&self) -> bool {
        self.psi_full_avg10 > 5.0
    }
}

// ─── /proc/meminfo parsing ──────────────────────────────────────────────────

/// Parse `MemTotal` and `MemAvailable` from `/proc/meminfo`.
/// Returns `(total_bytes, available_bytes)`.
#[cfg(target_os = "linux")]
fn parse_meminfo() -> Option<(u64, u64)> {
    let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo_contents(&contents)
}

fn parse_meminfo_contents(contents: &str) -> Option<(u64, u64)> {
    let mut total: Option<u64> = None;
    let mut available: Option<u64> = None;

    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = parse_kb_value(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available = parse_kb_value(rest);
        }
        if total.is_some() && available.is_some() {
            break;
        }
    }

    Some((total?, available?))
}

/// Parse a value like `"  16384000 kB"` into bytes.
fn parse_kb_value(s: &str) -> Option<u64> {
    let s = s.trim();
    let kb_str = s
        .strip_suffix("kB")
        .or_else(|| s.strip_suffix("KB"))?
        .trim();
    let kb: u64 = kb_str.parse().ok()?;
    Some(kb * 1024)
}

// ─── /proc/pressure/memory parsing ──────────────────────────────────────────

/// Parse PSI memory file. Returns `(some_avg10, full_avg10)`.
#[cfg(target_os = "linux")]
fn parse_psi() -> Option<(f64, f64)> {
    let contents = std::fs::read_to_string("/proc/pressure/memory").ok()?;
    parse_psi_contents(&contents)
}

fn parse_psi_contents(contents: &str) -> Option<(f64, f64)> {
    let mut some_avg10: Option<f64> = None;
    let mut full_avg10: Option<f64> = None;

    for line in contents.lines() {
        if line.starts_with("some ") {
            some_avg10 = extract_avg10(line);
        } else if line.starts_with("full ") {
            full_avg10 = extract_avg10(line);
        }
    }

    Some((some_avg10?, full_avg10?))
}

/// Extract `avg10=<value>` from a PSI line like:
/// `some avg10=0.00 avg60=0.00 avg300=0.00 total=123456`
fn extract_avg10(line: &str) -> Option<f64> {
    for token in line.split_whitespace() {
        if let Some(val) = token.strip_prefix("avg10=") {
            return val.parse().ok();
        }
    }
    None
}

// ─── cgroup memory limit ────────────────────────────────────────────────────

/// Try to read the cgroup memory limit. Checks cgroup v2 first, then v1.
/// Returns `None` if no limit is set or files are unreadable.
#[cfg(target_os = "linux")]
fn read_cgroup_limit() -> Option<u64> {
    // cgroup v2: /sys/fs/cgroup/memory.max
    if let Ok(contents) = std::fs::read_to_string("/sys/fs/cgroup/memory.max") {
        let trimmed = contents.trim();
        if trimmed != "max"
            && let Ok(val) = trimmed.parse::<u64>()
        {
            return Some(val);
        }
    }

    // cgroup v1: /sys/fs/cgroup/memory/memory.limit_in_bytes
    if let Ok(contents) = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes") {
        let trimmed = contents.trim();
        if let Ok(val) = trimmed.parse::<u64>() {
            // v1 reports a very large sentinel (PAGE_COUNTER_MAX * PAGE_SIZE) when
            // unlimited — treat anything above 2^62 as "no limit".
            if val < (1u64 << 62) {
                return Some(val);
            }
        }
    }

    None
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_meminfo_typical() {
        let input = "\
MemTotal:       65536000 kB
MemFree:        12345678 kB
MemAvailable:   40960000 kB
Buffers:          512000 kB
";
        let (total, available) = parse_meminfo_contents(input).unwrap();
        assert_eq!(total, 65_536_000 * 1024);
        assert_eq!(available, 40_960_000 * 1024);
    }

    #[test]
    fn parse_meminfo_missing_available_returns_none() {
        let input = "MemTotal:       65536000 kB\nMemFree:  1234 kB\n";
        assert!(parse_meminfo_contents(input).is_none());
    }

    #[test]
    fn parse_meminfo_missing_total_returns_none() {
        let input = "MemAvailable:   40960000 kB\n";
        assert!(parse_meminfo_contents(input).is_none());
    }

    #[test]
    fn parse_kb_value_basic() {
        assert_eq!(parse_kb_value("  16384 kB"), Some(16_384 * 1024));
        assert_eq!(parse_kb_value("0 kB"), Some(0));
    }

    #[test]
    fn parse_kb_value_invalid() {
        assert!(parse_kb_value("not a number kB").is_none());
        assert!(parse_kb_value("1234").is_none()); // no kB suffix
    }

    #[test]
    fn parse_psi_typical() {
        let input = "\
some avg10=1.23 avg60=0.45 avg300=0.12 total=999999
full avg10=0.05 avg60=0.01 avg300=0.00 total=111111
";
        let (some, full) = parse_psi_contents(input).unwrap();
        assert!((some - 1.23).abs() < f64::EPSILON);
        assert!((full - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_psi_missing_full_returns_none() {
        let input = "some avg10=1.23 avg60=0.45 avg300=0.12 total=999999\n";
        assert!(parse_psi_contents(input).is_none());
    }

    #[test]
    fn parse_psi_zero_values() {
        let input = "\
some avg10=0.00 avg60=0.00 avg300=0.00 total=0
full avg10=0.00 avg60=0.00 avg300=0.00 total=0
";
        let (some, full) = parse_psi_contents(input).unwrap();
        assert!((some - 0.0).abs() < f64::EPSILON);
        assert!((full - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn suggested_max_sessions_64gb() {
        let status = MemoryStatus {
            total_bytes: 64 * 1024 * 1024 * 1024,
            available_bytes: 50 * 1024 * 1024 * 1024,
            effective_limit_bytes: 64 * 1024 * 1024 * 1024,
            psi_some_avg10: 0.0,
            psi_full_avg10: 0.0,
        };
        // 64 * 0.70 = 44.8 GiB budget → 44 sessions
        assert_eq!(status.suggested_max_sessions(), 44);
    }

    #[test]
    fn suggested_max_sessions_respects_cgroup_limit() {
        let status = MemoryStatus {
            total_bytes: 64 * 1024 * 1024 * 1024,
            available_bytes: 50 * 1024 * 1024 * 1024,
            effective_limit_bytes: 4 * 1024 * 1024 * 1024, // 4 GiB container
            psi_some_avg10: 0.0,
            psi_full_avg10: 0.0,
        };
        // 4 * 0.70 = 2.8 GiB budget → 2 sessions
        assert_eq!(status.suggested_max_sessions(), 2);
    }

    #[test]
    fn suggested_max_sessions_minimum_one() {
        let status = MemoryStatus {
            total_bytes: 512 * 1024 * 1024, // 512 MiB
            available_bytes: 256 * 1024 * 1024,
            effective_limit_bytes: 512 * 1024 * 1024,
            psi_some_avg10: 0.0,
            psi_full_avg10: 0.0,
        };
        assert_eq!(status.suggested_max_sessions(), 1);
    }

    #[test]
    fn throttle_thresholds() {
        let ok = MemoryStatus {
            total_bytes: 64 * 1024 * 1024 * 1024,
            available_bytes: 50 * 1024 * 1024 * 1024,
            effective_limit_bytes: 64 * 1024 * 1024 * 1024,
            psi_some_avg10: 5.0,
            psi_full_avg10: 1.0,
        };
        assert!(!ok.should_throttle());
        assert!(!ok.is_critical());

        let throttle = MemoryStatus {
            psi_some_avg10: 20.0,
            ..ok.clone()
        };
        assert!(throttle.should_throttle());
        assert!(!throttle.is_critical());

        let critical = MemoryStatus {
            psi_full_avg10: 10.0,
            ..ok
        };
        assert!(critical.is_critical());
    }

    #[test]
    fn extract_avg10_parses_correctly() {
        assert_eq!(
            extract_avg10("some avg10=12.34 avg60=5.67 avg300=1.23 total=99"),
            Some(12.34)
        );
        assert_eq!(extract_avg10("no match here"), None);
    }
}

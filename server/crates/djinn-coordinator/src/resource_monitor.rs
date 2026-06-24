//! Compatibility shim: system resource monitoring.
//!
//! Reads `/proc/meminfo` for memory pressure detection.

#[derive(Debug, Clone)]
pub struct MemoryStatus {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub effective_limit_bytes: u64,
    pub psi_some_avg10: f64,
    pub psi_full_avg10: f64,
}

impl MemoryStatus {
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

    pub fn suggested_max_sessions(&self) -> u32 {
        let budget = (self.effective_limit_bytes as f64 * 0.70) as u64;
        let per_session: u64 = 1024 * 1024 * 1024;
        (budget / per_session).max(1) as u32
    }

    pub fn should_throttle(&self) -> bool {
        self.psi_some_avg10 > 15.0
    }

    pub fn is_critical(&self) -> bool {
        self.psi_full_avg10 > 50.0 || self.available_bytes < 256 * 1024 * 1024
    }
}

#[cfg(target_os = "linux")]
fn parse_meminfo() -> Option<(u64, u64)> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = 0u64;
    let mut available = 0u64;
    for line in content.lines() {
        if let Some(val) = line.strip_prefix("MemTotal:") {
            total = parse_kb(val).unwrap_or(0);
        } else if let Some(val) = line.strip_prefix("MemAvailable:") {
            available = parse_kb(val).unwrap_or(0);
        }
    }
    if total > 0 {
        Some((total, available))
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn parse_kb(s: &str) -> Option<u64> {
    let s = s.trim().strip_suffix("kB")?.trim();
    s.parse::<u64>().ok().map(|v| v * 1024)
}

#[cfg(target_os = "linux")]
fn read_cgroup_limit() -> Option<u64> {
    for path in [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ] {
        if let Ok(content) = std::fs::read_to_string(path) {
            let trimmed = content.trim();
            if trimmed == "max" {
                return None;
            }
            if let Ok(val) = trimmed.parse::<u64>() {
                return Some(val);
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn parse_psi() -> Option<(f64, f64)> {
    let content = std::fs::read_to_string("/proc/pressure/memory").ok()?;
    let mut some = 0.0f64;
    let mut full = 0.0f64;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("some ") {
            some = extract_avg10(rest);
        } else if let Some(rest) = line.strip_prefix("full ") {
            full = extract_avg10(rest);
        }
    }
    Some((some, full))
}

#[cfg(target_os = "linux")]
fn extract_avg10(s: &str) -> f64 {
    for part in s.split_whitespace() {
        if let Some(val) = part.strip_prefix("avg10=") {
            return val.parse().unwrap_or(0.0);
        }
    }
    0.0
}

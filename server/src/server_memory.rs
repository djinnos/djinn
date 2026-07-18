//! Scrape-time server process memory collection.
//!
//! The parser is pure so status-file fixtures cover Linux format changes and
//! failures without relying on the host `/proc` filesystem. Metric names and
//! bounded outcome handling remain in `djinn_telemetry::server_memory`.

use std::fmt;

const PROC_STATUS_PATH: &str = "/proc/self/status";
const KIB_BYTES: u64 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessMemory {
    rss_bytes: u64,
    anon_rss_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcStatusError {
    MissingVmRss,
    MissingRssAnon,
    MalformedVmRss,
    MalformedRssAnon,
    OverflowVmRss,
    OverflowRssAnon,
}

impl fmt::Display for ProcStatusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MissingVmRss => "VmRSS is missing from process status",
            Self::MissingRssAnon => "RssAnon is missing from process status",
            Self::MalformedVmRss => "VmRSS is malformed in process status",
            Self::MalformedRssAnon => "RssAnon is malformed in process status",
            Self::OverflowVmRss => "VmRSS overflows byte count",
            Self::OverflowRssAnon => "RssAnon overflows byte count",
        })
    }
}

impl std::error::Error for ProcStatusError {}

fn parse_proc_status(status: &str) -> Result<ProcessMemory, ProcStatusError> {
    let vm_rss = parse_kib_field(status, "VmRSS")?;
    let rss_anon = parse_kib_field(status, "RssAnon")?;
    Ok(ProcessMemory {
        rss_bytes: vm_rss,
        anon_rss_bytes: rss_anon,
    })
}

fn parse_kib_field(status: &str, field: &'static str) -> Result<u64, ProcStatusError> {
    let error = |kind| match (field, kind) {
        ("VmRSS", 0) => ProcStatusError::MissingVmRss,
        ("RssAnon", 0) => ProcStatusError::MissingRssAnon,
        ("VmRSS", 1) => ProcStatusError::MalformedVmRss,
        ("RssAnon", 1) => ProcStatusError::MalformedRssAnon,
        ("VmRSS", _) => ProcStatusError::OverflowVmRss,
        ("RssAnon", _) => ProcStatusError::OverflowRssAnon,
        _ => unreachable!("only fixed proc status fields are parsed"),
    };

    let value = status
        .lines()
        .find_map(|line| {
            line.strip_prefix(field)
                .and_then(|rest| rest.strip_prefix(':'))
        })
        .ok_or_else(|| error(0))?;
    let mut parts = value.split_ascii_whitespace();
    let kib = parts.next().ok_or_else(|| error(1))?;
    if parts.next() != Some("kB") || parts.next().is_some() {
        return Err(error(1));
    }
    kib.parse::<u64>()
        .map_err(|_| error(1))?
        .checked_mul(KIB_BYTES)
        .ok_or_else(|| error(2))
}

/// Refresh bounded process and allocator memory gauges without failing a scrape.
pub fn refresh() {
    match std::fs::read_to_string(PROC_STATUS_PATH)
        .and_then(|status| parse_proc_status(&status).map_err(std::io::Error::other))
    {
        Ok(memory) => djinn_telemetry::server_memory::record_process_rss(
            memory.rss_bytes,
            memory.anon_rss_bytes,
        ),
        Err(error) => {
            djinn_telemetry::server_memory::record_process_unavailable();
            tracing::debug!(error = %error, "server process memory status is unavailable");
        }
    }

    refresh_jemalloc();
}

#[cfg(target_os = "linux")]
fn refresh_jemalloc() {
    match crate::allocator::stats() {
        Ok(stats) => djinn_telemetry::server_memory::record_jemalloc_stats(
            stats.allocated,
            stats.resident,
            stats.retained,
        ),
        Err(error) => {
            djinn_telemetry::server_memory::record_jemalloc_unavailable();
            tracing::debug!(error = %error, "jemalloc memory statistics are unavailable");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn refresh_jemalloc() {
    djinn_telemetry::server_memory::record_jemalloc_unavailable();
}

#[cfg(test)]
mod tests {
    use super::{ProcStatusError, ProcessMemory, parse_proc_status};

    #[test]
    fn parses_vm_rss_and_anonymous_rss_in_kib() {
        let memory = parse_proc_status("Name:\tdjinn\nVmRSS:\t  123 kB\nRssAnon:\t45 kB\n")
            .expect("valid proc status");
        assert_eq!(
            memory,
            ProcessMemory {
                rss_bytes: 123 * 1024,
                anon_rss_bytes: 45 * 1024,
            }
        );
    }

    #[test]
    fn rejects_missing_memory_fields() {
        assert_eq!(
            parse_proc_status("VmRSS:\t123 kB\n"),
            Err(ProcStatusError::MissingRssAnon)
        );
        assert_eq!(
            parse_proc_status("RssAnon:\t123 kB\n"),
            Err(ProcStatusError::MissingVmRss)
        );
    }

    #[test]
    fn rejects_malformed_memory_fields() {
        assert_eq!(
            parse_proc_status("VmRSS:\tnot-a-number kB\nRssAnon:\t123 kB\n"),
            Err(ProcStatusError::MalformedVmRss)
        );
        assert_eq!(
            parse_proc_status("VmRSS:\t123 kB\nRssAnon:\t123 bytes\n"),
            Err(ProcStatusError::MalformedRssAnon)
        );
    }

    #[test]
    fn memory_telemetry_renders_fixed_outcomes_and_byte_gauges() {
        djinn_telemetry::init().expect("telemetry init");
        djinn_telemetry::server_memory::record_process_rss(2_048, 1_024);
        djinn_telemetry::server_memory::record_jemalloc_stats(512, 1_024, 2_048);
        djinn_telemetry::server_memory::record_jemalloc_unavailable();

        let rendered = djinn_telemetry::render().expect("render server-memory telemetry");
        for (metric, value) in [
            ("djinn_process_rss_bytes", "2048"),
            ("djinn_process_anon_rss_bytes", "1024"),
            ("djinn_jemalloc_allocated_bytes", "512"),
            ("djinn_jemalloc_resident_bytes", "1024"),
            ("djinn_jemalloc_retained_bytes", "2048"),
        ] {
            assert!(
                rendered.contains(&format!("{metric} {value}")),
                "missing byte gauge {metric} in:\n{rendered}"
            );
        }
        assert!(rendered.lines().any(|line| {
            line.starts_with("djinn_server_memory_scrape_outcome{")
                && line.contains("source=\"jemalloc\"")
                && line.contains("outcome=\"unavailable\"")
                && line.ends_with(" 1")
        }));
    }
}

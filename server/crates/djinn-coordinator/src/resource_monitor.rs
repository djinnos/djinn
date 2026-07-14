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

// ─── /proc/pressure parsing ─────────────────────────────────────────────────

/// One PSI sampling pass, with a separate result for every kernel resource.
///
/// Values are kernel PSI percentages, not Prometheus ratios. Errors are the
/// bounded [`djinn_telemetry::psi::REASON_*`] values used by the telemetry
/// producer. Keeping the results independent means a partially supported
/// kernel still reports pressure for the resources it does expose.
#[derive(Debug, Clone, PartialEq)]
pub struct PsiSamples {
    pub cpu: Result<f64, &'static str>,
    pub memory: Result<f64, &'static str>,
    pub io: Result<f64, &'static str>,
}

/// Read the three Linux PSI files through `read_file`.
///
/// The injected boundary makes this operation deterministic in tests and lets
/// callers handle each resource independently. `read_file` is called exactly
/// once for each path in CPU, memory, IO order.
pub fn sample_psi_with(mut read_file: impl FnMut(&str) -> std::io::Result<String>) -> PsiSamples {
    PsiSamples {
        cpu: sample_psi_resource(&mut read_file, "/proc/pressure/cpu"),
        memory: sample_psi_resource(&mut read_file, "/proc/pressure/memory"),
        io: sample_psi_resource(&mut read_file, "/proc/pressure/io"),
    }
}

/// Sample PSI from the host's procfs files.
#[cfg(target_os = "linux")]
pub fn sample_psi() -> PsiSamples {
    sample_psi_with(|path| std::fs::read_to_string(path))
}

/// A non-Linux host does not expose Linux procfs PSI files.
#[cfg(not(target_os = "linux"))]
pub fn sample_psi() -> PsiSamples {
    sample_psi_with(|_| Err(std::io::Error::from(std::io::ErrorKind::NotFound)))
}

/// Publish one independent PSI sampling pass through the telemetry helpers.
///
/// Each successful resource publishes its `some avg10` kernel percentage as a
/// ratio gauge (availability 1). Each failed resource sets availability to 0,
/// replaces its stale pressure value with NaN, and increments exactly one
/// bounded error counter. Resources are published independently, so a partially
/// supported kernel still reports the resources it does expose.
///
/// This function is best-effort and synchronous: it never returns an error and
/// never panics, so a read/parse failure cannot stop repeated monitor sampling.
pub fn publish_psi(samples: &PsiSamples) {
    publish_psi_resource(djinn_telemetry::psi::RESOURCE_CPU, &samples.cpu);
    publish_psi_resource(djinn_telemetry::psi::RESOURCE_MEMORY, &samples.memory);
    publish_psi_resource(djinn_telemetry::psi::RESOURCE_IO, &samples.io);
}

fn publish_psi_resource(resource: &'static str, result: &Result<f64, &'static str>) {
    match result {
        Ok(percent) => djinn_telemetry::psi::record_success(resource, *percent),
        Err(reason) => djinn_telemetry::psi::record_failure(resource, reason),
    }
}

/// Sample PSI from procfs and publish the results through telemetry helpers.
///
/// Called once per coordinator resource-sampling pass. Combines
/// [`sample_psi`] and [`publish_psi`] so the live monitor path is a single
/// non-failing call.
pub fn sample_and_publish_psi() {
    let samples = sample_psi();
    publish_psi(&samples);
}

fn sample_psi_resource(
    read_file: &mut impl FnMut(&str) -> std::io::Result<String>,
    path: &str,
) -> Result<f64, &'static str> {
    match read_file(path) {
        Ok(contents) => parse_some_avg10(&contents).ok_or(djinn_telemetry::psi::REASON_PARSE),
        Err(error) => Err(classify_psi_read_error(error.kind())),
    }
}

fn classify_psi_read_error(kind: std::io::ErrorKind) -> &'static str {
    match kind {
        std::io::ErrorKind::NotFound => djinn_telemetry::psi::REASON_MISSING,
        std::io::ErrorKind::PermissionDenied => djinn_telemetry::psi::REASON_PERMISSION,
        _ => djinn_telemetry::psi::REASON_IO,
    }
}

/// Parse the `some` line's `avg10` field from a PSI file.
///
/// CPU PSI files normally do not have a `full` line, so a valid `some` line is
/// sufficient. PSI values must be finite kernel percentages.
fn parse_some_avg10(contents: &str) -> Option<f64> {
    contents
        .lines()
        .find_map(|line| line.strip_prefix("some ").and_then(extract_avg10))
}

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
            let value: f64 = val.parse().ok()?;
            return value.is_finite().then_some(value);
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
    fn psi_sampling_keeps_resources_independent_and_uses_kernel_percentages() {
        let samples = sample_psi_with(|path| match path {
            "/proc/pressure/cpu" => Ok("some avg10=12.50 avg60=0.00 total=1\n".to_owned()),
            "/proc/pressure/memory" => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            "/proc/pressure/io" => Ok("some avg10=0.25 avg60=0.00 total=1\n".to_owned()),
            _ => Err(std::io::Error::from(std::io::ErrorKind::Other)),
        });

        assert_eq!(samples.cpu, Ok(12.50));
        assert_eq!(samples.memory, Err(djinn_telemetry::psi::REASON_MISSING));
        assert_eq!(samples.io, Ok(0.25));
    }

    #[test]
    fn psi_sampling_classifies_read_errors() {
        let samples = sample_psi_with(|path| match path {
            "/proc/pressure/cpu" => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            "/proc/pressure/memory" => {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            }
            "/proc/pressure/io" => Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
            _ => Err(std::io::Error::from(std::io::ErrorKind::Other)),
        });

        assert_eq!(samples.cpu, Err(djinn_telemetry::psi::REASON_MISSING));
        assert_eq!(samples.memory, Err(djinn_telemetry::psi::REASON_PERMISSION));
        assert_eq!(samples.io, Err(djinn_telemetry::psi::REASON_IO));
    }

    #[test]
    fn psi_sampling_classifies_invalid_some_avg10_as_parse_errors() {
        for contents in [
            "some avg10=not-a-number avg60=0.00 total=1\n",
            "full avg10=0.00 avg60=0.00 total=1\n",
            "some avg60=0.00 total=1\n",
            "some avg10=NaN avg60=0.00 total=1\n",
            "some avg10=inf avg60=0.00 total=1\n",
        ] {
            let samples = sample_psi_with(|_| Ok(contents.to_owned()));
            assert_eq!(samples.cpu, Err(djinn_telemetry::psi::REASON_PARSE));
            assert_eq!(samples.memory, Err(djinn_telemetry::psi::REASON_PARSE));
            assert_eq!(samples.io, Err(djinn_telemetry::psi::REASON_PARSE));
        }
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

    // ─── PSI publication (live monitor wiring) tests ────────────────────────

    /// Metric names are private to djinn_telemetry, so these mirror the
    /// documented exported Prometheus names.
    const PSI_RATIO_METRIC: &str = "node_psi_some_avg10_ratio";
    const PSI_AVAILABLE_METRIC: &str = "node_psi_available";
    const PSI_ERRORS_METRIC: &str = "node_psi_read_errors_total";

    /// Serializes telemetry-rendering PSI tests so their before/after deltas
    /// are not perturbed by a parallel test mutating the process-global
    /// registry. Mirrors the global `TEST_MUTEX` pattern in djinn-telemetry.
    static PSI_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn psi_rendered_sample<'a>(
        rendered: &'a str,
        metric: &str,
        labels: &[(&str, &str)],
    ) -> &'a str {
        rendered
            .lines()
            .find(|line| {
                line.starts_with(metric)
                    && labels
                        .iter()
                        .all(|(key, value)| line.contains(&format!("{key}=\"{value}\"")))
            })
            .unwrap_or_else(|| panic!("missing sample {metric}{labels:?} in:\n{rendered}"))
    }

    fn psi_labeled_value(rendered: &str, metric: &str, labels: &[(&str, &str)]) -> f64 {
        let line = psi_rendered_sample(rendered, metric, labels);
        line.rsplit_once(' ')
            .and_then(|(_, v)| v.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("labeled sample should end with a number: {line}"))
    }

    #[test]
    fn publish_psi_partial_support_publishes_independent_resources() {
        let _guard = PSI_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        djinn_telemetry::init().unwrap();

        // CPU succeeds, memory fails (parse), IO succeeds.
        let samples = PsiSamples {
            cpu: Ok(12.5),
            memory: Err(djinn_telemetry::psi::REASON_PARSE),
            io: Ok(0.5),
        };
        let errors_before = psi_labeled_value(
            &djinn_telemetry::render().unwrap(),
            PSI_ERRORS_METRIC,
            &[("resource", "memory"), ("reason", "parse")],
        );
        publish_psi(&samples);
        let rendered = djinn_telemetry::render().unwrap();

        // Successful resources publish the ratio and availability 1.
        assert_eq!(
            psi_labeled_value(&rendered, PSI_RATIO_METRIC, &[("resource", "cpu")]),
            0.125
        );
        assert_eq!(
            psi_labeled_value(&rendered, PSI_AVAILABLE_METRIC, &[("resource", "cpu")]),
            1.0
        );
        assert_eq!(
            psi_labeled_value(&rendered, PSI_RATIO_METRIC, &[("resource", "io")]),
            0.005
        );
        assert_eq!(
            psi_labeled_value(&rendered, PSI_AVAILABLE_METRIC, &[("resource", "io")]),
            1.0
        );

        // Failed resource: availability 0, stale ratio replaced with NaN.
        assert_eq!(
            psi_labeled_value(&rendered, PSI_AVAILABLE_METRIC, &[("resource", "memory")]),
            0.0
        );
        let mem_ratio = psi_rendered_sample(&rendered, PSI_RATIO_METRIC, &[("resource", "memory")]);
        assert!(
            mem_ratio.ends_with(" NaN"),
            "PSI failure must replace stale value with NaN: {mem_ratio}"
        );

        // Exactly one bounded error counter increment for the failed resource.
        assert_eq!(
            psi_labeled_value(
                &rendered,
                PSI_ERRORS_METRIC,
                &[("resource", "memory"), ("reason", "parse")]
            ),
            errors_before + 1.0
        );

        // The failed resource did not perturb successful resources.
        assert_eq!(
            psi_labeled_value(&rendered, PSI_RATIO_METRIC, &[("resource", "cpu")]),
            0.125
        );
    }

    #[test]
    fn publish_psi_recovers_from_failure_to_valid_sample() {
        let _guard = PSI_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        djinn_telemetry::init().unwrap();

        // First pass: CPU fails.
        publish_psi(&PsiSamples {
            cpu: Err(djinn_telemetry::psi::REASON_MISSING),
            memory: Ok(40.0),
            io: Ok(0.0),
        });
        let after_failure = djinn_telemetry::render().unwrap();
        assert_eq!(
            psi_labeled_value(&after_failure, PSI_AVAILABLE_METRIC, &[("resource", "cpu")]),
            0.0
        );

        // Second pass: CPU recovers with a valid sample.
        publish_psi(&PsiSamples {
            cpu: Ok(25.0),
            memory: Ok(40.0),
            io: Ok(0.0),
        });
        let after_recovery = djinn_telemetry::render().unwrap();
        assert_eq!(
            psi_labeled_value(&after_recovery, PSI_RATIO_METRIC, &[("resource", "cpu")]),
            0.25
        );
        assert_eq!(
            psi_labeled_value(
                &after_recovery,
                PSI_AVAILABLE_METRIC,
                &[("resource", "cpu")]
            ),
            1.0
        );
    }

    #[test]
    fn publish_psi_repeated_failures_do_not_panic_or_suppress_recovery() {
        let _guard = PSI_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        djinn_telemetry::init().unwrap();

        // Repeated sampling with all resources failing must not panic and must
        // be safe to call many times (the monitor loop runs on every tick).
        for _ in 0..3 {
            publish_psi(&PsiSamples {
                cpu: Err(djinn_telemetry::psi::REASON_MISSING),
                memory: Err(djinn_telemetry::psi::REASON_PERMISSION),
                io: Err(djinn_telemetry::psi::REASON_IO),
            });
        }

        // A later valid sample must restore availability and the ratio.
        publish_psi(&PsiSamples {
            cpu: Ok(10.0),
            memory: Ok(20.0),
            io: Ok(5.0),
        });
        let rendered = djinn_telemetry::render().unwrap();
        for resource in ["cpu", "memory", "io"] {
            assert_eq!(
                psi_labeled_value(&rendered, PSI_AVAILABLE_METRIC, &[("resource", resource)]),
                1.0
            );
        }
        assert_eq!(
            psi_labeled_value(&rendered, PSI_RATIO_METRIC, &[("resource", "io")]),
            0.05
        );
    }

    #[test]
    fn publish_psi_does_not_break_memory_status_consumer_path() {
        let _guard = PSI_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        djinn_telemetry::init().unwrap();

        // Publishing a failed PSI pass must not panic or affect the
        // MemoryStatus-based throttling path: a MemoryStatus constructed from
        // its fields still reports correct throttle/critical decisions.
        publish_psi(&PsiSamples {
            cpu: Err(djinn_telemetry::psi::REASON_PARSE),
            memory: Err(djinn_telemetry::psi::REASON_MISSING),
            io: Err(djinn_telemetry::psi::REASON_IO),
        });

        let status = MemoryStatus {
            total_bytes: 64 * 1024 * 1024 * 1024,
            available_bytes: 50 * 1024 * 1024 * 1024,
            effective_limit_bytes: 64 * 1024 * 1024 * 1024,
            psi_some_avg10: 5.0,
            psi_full_avg10: 1.0,
        };
        assert!(!status.should_throttle());
        assert!(!status.is_critical());

        let throttled = MemoryStatus {
            psi_some_avg10: 20.0,
            ..status
        };
        assert!(throttled.should_throttle());
    }
}

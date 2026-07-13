//! Retrieval-health doctor checks.
//!
//! This module provides the database-independent, pure-synchronous core of the
//! retrieval-health detector. All database access and control-plane wiring live
//! outside `djinn-core`; this crate only defines the immutable config, snapshot
//! types, data-source abstraction, and the `DoctorCheck` implementation.
//!
//! The check emits [`memory.retrieval_zero_result`](RETRIEVAL_ZERO_RESULT_NAME)
//! per project only when the query count in the window is at least the
//! configured floor and the zero-result rate is strictly greater than the
//! configured threshold. Equality against the threshold is considered healthy.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use time::OffsetDateTime;

use crate::doctor::{
    DoctorCheck, DoctorCheckCadence, DoctorResult, Finding, FindingSeverity, ResolverSnapshot,
};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Minimum supported retrieval-health window, in whole hours.
pub const MIN_WINDOW_HOURS: u64 = 1;

/// Maximum supported retrieval-health window, in whole hours (7 days).
pub const MAX_WINDOW_HOURS: u64 = 168;

/// Minimum supported zero-result threshold (inclusive).
pub const MIN_ZERO_RESULT_THRESHOLD: f64 = 0.0;

/// Maximum supported zero-result threshold (inclusive).
pub const MAX_ZERO_RESULT_THRESHOLD: f64 = 1.0;

/// Minimum supported query floor (inclusive).
pub const MIN_QUERY_FLOOR: u64 = 1;

/// Maximum supported query floor (inclusive).
pub const MAX_QUERY_FLOOR: u64 = 10_000_000;

/// Default retrieval-health window: 24 hours.
pub const DEFAULT_WINDOW_HOURS: u64 = 24;

/// Default zero-result threshold: 0.50.
pub const DEFAULT_ZERO_RESULT_THRESHOLD: f64 = 0.50;

/// Default query floor: 20 queries.
pub const DEFAULT_QUERY_FLOOR: u64 = 20;

/// Errors returned when constructing an invalid [`RetrievalHealthConfig`].
#[derive(Debug, Error, PartialEq)]
pub enum RetrievalHealthConfigError {
    /// `window_hours` was outside the documented bounds.
    #[error(
        "window_hours {0} is outside the supported range [{MIN_WINDOW_HOURS}, {MAX_WINDOW_HOURS}]"
    )]
    WindowHours(u64),

    /// `zero_result_threshold` was outside the documented `[0.0, 1.0]` bounds.
    #[error(
        "zero_result_threshold {0} is outside the supported range [{MIN_ZERO_RESULT_THRESHOLD}, {MAX_ZERO_RESULT_THRESHOLD}]"
    )]
    Threshold(f64),

    /// `query_floor` was outside the documented bounds.
    #[error(
        "query_floor {0} is outside the supported range [{MIN_QUERY_FLOOR}, {MAX_QUERY_FLOOR}]"
    )]
    QueryFloor(u64),

    /// An environment variable was set to a non-numeric value.
    #[error("environment variable {var} has invalid value {value}")]
    InvalidEnvValue { var: String, value: String },
}

/// Immutable configuration for retrieval-health checks.
///
/// All values are validated at construction time. Defaults are a 24-hour
/// window, 0.50 zero-result threshold, and a 20-query floor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetrievalHealthConfig {
    window_hours: u64,
    zero_result_threshold: f64,
    query_floor: u64,
}

impl RetrievalHealthConfig {
    /// Construct a config from validated values.
    ///
    /// Returns a typed error for any value outside the documented bounds:
    ///
    /// * `window_hours`: [`MIN_WINDOW_HOURS`] to [`MAX_WINDOW_HOURS`]
    /// * `zero_result_threshold`: [`MIN_ZERO_RESULT_THRESHOLD`] to
    ///   [`MAX_ZERO_RESULT_THRESHOLD`]
    /// * `query_floor`: [`MIN_QUERY_FLOOR`] to [`MAX_QUERY_FLOOR`]
    pub fn new(
        window_hours: u64,
        zero_result_threshold: f64,
        query_floor: u64,
    ) -> Result<Self, RetrievalHealthConfigError> {
        if !(MIN_WINDOW_HOURS..=MAX_WINDOW_HOURS).contains(&window_hours) {
            return Err(RetrievalHealthConfigError::WindowHours(window_hours));
        }
        if !(MIN_ZERO_RESULT_THRESHOLD..=MAX_ZERO_RESULT_THRESHOLD).contains(&zero_result_threshold)
        {
            return Err(RetrievalHealthConfigError::Threshold(zero_result_threshold));
        }
        if !(MIN_QUERY_FLOOR..=MAX_QUERY_FLOOR).contains(&query_floor) {
            return Err(RetrievalHealthConfigError::QueryFloor(query_floor));
        }
        Ok(Self {
            window_hours,
            zero_result_threshold,
            query_floor,
        })
    }

    /// The configured window, in whole hours.
    pub fn window_hours(&self) -> u64 {
        self.window_hours
    }

    /// The configured zero-result threshold.
    pub fn zero_result_threshold(&self) -> f64 {
        self.zero_result_threshold
    }

    /// The configured minimum query floor before a finding may be emitted.
    pub fn query_floor(&self) -> u64 {
        self.query_floor
    }

    /// Parse a config from the documented environment variables.
    ///
    /// Variables:
    /// * `DJINN_RETRIEVAL_HEALTH_WINDOW_HOURS` — defaults to 24.
    /// * `DJINN_RETRIEVAL_HEALTH_ZERO_RESULT_THRESHOLD` — defaults to 0.50.
    /// * `DJINN_RETRIEVAL_HEALTH_QUERY_FLOOR` — defaults to 20.
    ///
    /// Any explicitly set value outside the documented bounds returns an error.
    pub fn from_env() -> Result<Self, RetrievalHealthConfigError> {
        let window_hours =
            parse_env_u64("DJINN_RETRIEVAL_HEALTH_WINDOW_HOURS", DEFAULT_WINDOW_HOURS)?;
        let zero_result_threshold = parse_env_f64(
            "DJINN_RETRIEVAL_HEALTH_ZERO_RESULT_THRESHOLD",
            DEFAULT_ZERO_RESULT_THRESHOLD,
        )?;
        let query_floor = parse_env_u64("DJINN_RETRIEVAL_HEALTH_QUERY_FLOOR", DEFAULT_QUERY_FLOOR)?;
        Self::new(window_hours, zero_result_threshold, query_floor)
    }
}

impl Default for RetrievalHealthConfig {
    fn default() -> Self {
        Self::new(
            DEFAULT_WINDOW_HOURS,
            DEFAULT_ZERO_RESULT_THRESHOLD,
            DEFAULT_QUERY_FLOOR,
        )
        .expect("defaults are within documented bounds")
    }
}

fn parse_env_u64(var: &str, default: u64) -> Result<u64, RetrievalHealthConfigError> {
    match std::env::var(var) {
        Ok(value) => {
            value
                .parse::<u64>()
                .map_err(|_| RetrievalHealthConfigError::InvalidEnvValue {
                    var: var.to_string(),
                    value,
                })
        }
        Err(_) => Ok(default),
    }
}

fn parse_env_f64(var: &str, default: f64) -> Result<f64, RetrievalHealthConfigError> {
    match std::env::var(var) {
        Ok(value) => {
            value
                .parse::<f64>()
                .map_err(|_| RetrievalHealthConfigError::InvalidEnvValue {
                    var: var.to_string(),
                    value,
                })
        }
        Err(_) => Ok(default),
    }
}

// ---------------------------------------------------------------------------
// Snapshot types
// ---------------------------------------------------------------------------

/// Counts for a single retrieval entry point within a project window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryPointCounts {
    /// Total number of completed queries for this entry point.
    pub total_queries: u64,
    /// Number of completed queries that returned zero results.
    pub zero_result_queries: u64,
}

impl EntryPointCounts {
    /// Zero counts, useful when an entry point is present but had no traffic.
    pub const fn zero() -> Self {
        Self {
            total_queries: 0,
            zero_result_queries: 0,
        }
    }
}

/// Immutable snapshot of retrieval-health counts for a single project over an
/// exact window.
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalProjectWindowSnapshot {
    /// Project identifier the snapshot covers.
    pub project_id: String,
    /// Inclusive start of the half-open window.
    pub window_start: OffsetDateTime,
    /// Exclusive end of the half-open window.
    pub window_end: OffsetDateTime,
    /// Per-entry-point counts within the window.
    pub entry_point_counts: BTreeMap<String, EntryPointCounts>,
}

/// Immutable snapshot covering all projects for a single check invocation.
///
/// The control plane is responsible for prefetching the per-project rollups and
/// adapting them into this shape before invoking the pure core check.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RetrievalHealthSnapshot {
    /// Project snapshots keyed by project id.
    pub projects: BTreeMap<String, RetrievalProjectWindowSnapshot>,
}

// ---------------------------------------------------------------------------
// Data source
// ---------------------------------------------------------------------------

/// Synchronous, immutable data source for retrieval health.
///
/// Implementations are expected to return an already-prefetched snapshot; the
/// check itself performs no I/O. This keeps `djinn-core` free of database and
/// control-plane dependencies while still allowing a production adapter to feed
/// the check from `djinn-db` rollups and process metrics.
pub trait RetrievalHealthDataSource: Send + Sync {
    /// Return the immutable snapshot for the current check invocation.
    fn snapshot(&self) -> RetrievalHealthSnapshot;
}

// ---------------------------------------------------------------------------
// Check
// ---------------------------------------------------------------------------

/// Stable name for the zero-result retrieval finding.
pub const RETRIEVAL_ZERO_RESULT_NAME: &str = "memory.retrieval_zero_result";

/// Pure synchronous doctor check that flags projects whose zero-result rate is
/// strictly above the configured threshold over the configured window.
///
/// The check takes an immutable config and a synchronous data source. It is
/// deliberately on-demand (not cheap-periodic) because each invocation may
/// require a fresh prefetched snapshot from the control plane.
pub struct RetrievalZeroResultCheck {
    config: RetrievalHealthConfig,
    source: Arc<dyn RetrievalHealthDataSource>,
}

impl RetrievalZeroResultCheck {
    /// Construct the check.
    pub fn new(config: RetrievalHealthConfig, source: Arc<dyn RetrievalHealthDataSource>) -> Self {
        Self { config, source }
    }
}

impl DoctorCheck for RetrievalZeroResultCheck {
    fn name(&self) -> &'static str {
        RETRIEVAL_ZERO_RESULT_NAME
    }

    fn description(&self) -> &'static str {
        "Flags projects whose memory retrieval zero-result rate is strictly above the configured threshold"
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        let snapshot = self.source.snapshot();
        let mut findings = Vec::with_capacity(snapshot.projects.len());
        for project in snapshot.projects.values() {
            if let Some(finding) = resolve_retrieval_zero_result(project, &self.config) {
                findings.push(finding);
            }
        }
        Ok(findings)
    }

    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::OnDemand
    }
}

// ---------------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------------

fn resolve_retrieval_zero_result(
    project: &RetrievalProjectWindowSnapshot,
    config: &RetrievalHealthConfig,
) -> Option<Finding> {
    let mut total_queries: u64 = 0;
    let mut zero_result_queries: u64 = 0;
    for counts in project.entry_point_counts.values() {
        total_queries += counts.total_queries;
        zero_result_queries += counts.zero_result_queries;
    }

    let below_floor = total_queries < config.query_floor();
    let rate = if total_queries > 0 {
        zero_result_queries as f64 / total_queries as f64
    } else {
        0.0
    };
    // Strictly greater than threshold; equality passes.
    let above_threshold = rate > config.zero_result_threshold();

    let entry_point_counts_json = serde_json::to_value(&project.entry_point_counts)
        .expect("EntryPointCounts serializes to JSON");

    let inputs = json!({
        "project_id": project.project_id,
        "window_start": iso_format(project.window_start),
        "window_end": iso_format(project.window_end),
        "threshold": config.zero_result_threshold(),
        "floor": config.query_floor(),
        "entry_point_counts": entry_point_counts_json,
        "total_queries": total_queries,
        "zero_result_queries": zero_result_queries,
        "rate": rate,
        "below_floor": below_floor,
        "above_threshold": above_threshold,
    });

    let outputs = json!({
        "below_floor": below_floor,
        "above_threshold": above_threshold,
        "rate": rate,
        "total_queries": total_queries,
        "zero_result_queries": zero_result_queries,
    });

    if below_floor || !above_threshold {
        return None;
    }

    let evidence = json!({
        "project_id": project.project_id,
        "window": {
            "start": iso_format(project.window_start),
            "end": iso_format(project.window_end),
        },
        "threshold": config.zero_result_threshold(),
        "floor": config.query_floor(),
        "numerator": zero_result_queries,
        "denominator": total_queries,
        "rate": rate,
        "per_entry_point_counts": entry_point_counts_json,
    });

    Some(
        Finding::new(
            FindingSeverity::Warn,
            RETRIEVAL_ZERO_RESULT_NAME,
            ResolverSnapshot::new("resolve_retrieval_zero_result", inputs, outputs),
            format!(
                "project {} has a zero-result rate of {:.4} ({} / {}), above the {} threshold over the last {} hours",
                project.project_id,
                rate,
                zero_result_queries,
                total_queries,
                config.zero_result_threshold(),
                config.window_hours()
            ),
        )
        .with_entity_id("project_id", &project.project_id)
        .with_evidence(evidence),
    )
}

/// Format an [`OffsetDateTime`] as an ISO-8601 string for JSON snapshots.
fn iso_format(ts: OffsetDateTime) -> String {
    ts.format(&time::format_description::well_known::Iso8601::DEFAULT)
        .unwrap_or_else(|_| ts.unix_timestamp().to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // -----------------------------------------------------------------
    // In-memory test double
    // -----------------------------------------------------------------

    #[derive(Default)]
    struct MemoryDataSource {
        snapshot: RetrievalHealthSnapshot,
    }

    impl RetrievalHealthDataSource for MemoryDataSource {
        fn snapshot(&self) -> RetrievalHealthSnapshot {
            self.snapshot.clone()
        }
    }

    fn ts(hour: i64) -> OffsetDateTime {
        // Use a fixed base date so tests are deterministic.
        OffsetDateTime::from_unix_timestamp(1_704_000_000).unwrap() + time::Duration::hours(hour)
    }

    fn make_project(
        project_id: &str,
        total: u64,
        zero: u64,
        entry_points: Vec<(&str, u64, u64)>,
    ) -> RetrievalProjectWindowSnapshot {
        let mut counts = BTreeMap::new();
        if entry_points.is_empty() {
            counts.insert(
                "memory_search".to_string(),
                EntryPointCounts {
                    total_queries: total,
                    zero_result_queries: zero,
                },
            );
        } else {
            for (name, ep_total, ep_zero) in entry_points {
                counts.insert(
                    name.to_string(),
                    EntryPointCounts {
                        total_queries: ep_total,
                        zero_result_queries: ep_zero,
                    },
                );
            }
        }
        RetrievalProjectWindowSnapshot {
            project_id: project_id.to_string(),
            window_start: ts(0),
            window_end: ts(24),
            entry_point_counts: counts,
        }
    }

    fn make_project_with_total_zero(
        project_id: &str,
        total: u64,
        zero: u64,
    ) -> RetrievalProjectWindowSnapshot {
        make_project(project_id, total, zero, vec![])
    }

    fn run_check(
        config: RetrievalHealthConfig,
        projects: Vec<RetrievalProjectWindowSnapshot>,
    ) -> Vec<Finding> {
        let mut snapshot = RetrievalHealthSnapshot::default();
        for project in projects {
            snapshot
                .projects
                .insert(project.project_id.clone(), project);
        }
        let source: Arc<dyn RetrievalHealthDataSource> = Arc::new(MemoryDataSource { snapshot });
        let check = RetrievalZeroResultCheck::new(config, source);
        check.run().expect("run should succeed")
    }

    // -----------------------------------------------------------------
    // Config validation
    // -----------------------------------------------------------------

    #[test]
    fn defaults_match_spec() {
        let config = RetrievalHealthConfig::default();
        assert_eq!(config.window_hours(), 24);
        assert_eq!(config.zero_result_threshold(), 0.50);
        assert_eq!(config.query_floor(), 20);
    }

    #[test]
    fn valid_config_endpoints() {
        assert!(RetrievalHealthConfig::new(1, 0.0, 1).is_ok());
        assert!(RetrievalHealthConfig::new(168, 1.0, 10_000_000).is_ok());
    }

    #[test]
    fn invalid_window_hours_bounds() {
        assert_eq!(
            RetrievalHealthConfig::new(0, DEFAULT_ZERO_RESULT_THRESHOLD, DEFAULT_QUERY_FLOOR),
            Err(RetrievalHealthConfigError::WindowHours(0))
        );
        assert_eq!(
            RetrievalHealthConfig::new(169, DEFAULT_ZERO_RESULT_THRESHOLD, DEFAULT_QUERY_FLOOR),
            Err(RetrievalHealthConfigError::WindowHours(169))
        );
    }

    #[test]
    fn invalid_threshold_bounds() {
        assert_eq!(
            RetrievalHealthConfig::new(DEFAULT_WINDOW_HOURS, -0.1, DEFAULT_QUERY_FLOOR),
            Err(RetrievalHealthConfigError::Threshold(-0.1))
        );
        assert_eq!(
            RetrievalHealthConfig::new(DEFAULT_WINDOW_HOURS, 1.1, DEFAULT_QUERY_FLOOR),
            Err(RetrievalHealthConfigError::Threshold(1.1))
        );
    }

    #[test]
    fn invalid_query_floor_bounds() {
        assert_eq!(
            RetrievalHealthConfig::new(DEFAULT_WINDOW_HOURS, DEFAULT_ZERO_RESULT_THRESHOLD, 0),
            Err(RetrievalHealthConfigError::QueryFloor(0))
        );
        assert_eq!(
            RetrievalHealthConfig::new(
                DEFAULT_WINDOW_HOURS,
                DEFAULT_ZERO_RESULT_THRESHOLD,
                10_000_001
            ),
            Err(RetrievalHealthConfigError::QueryFloor(10_000_001))
        );
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn clear_env_vars() {
        for var in [
            "DJINN_RETRIEVAL_HEALTH_WINDOW_HOURS",
            "DJINN_RETRIEVAL_HEALTH_ZERO_RESULT_THRESHOLD",
            "DJINN_RETRIEVAL_HEALTH_QUERY_FLOOR",
        ] {
            unsafe {
                let _ = std::env::remove_var(var);
            }
        }
    }

    fn set_env_vars(window: &str, threshold: &str, floor: &str) {
        unsafe {
            std::env::set_var("DJINN_RETRIEVAL_HEALTH_WINDOW_HOURS", window);
            std::env::set_var("DJINN_RETRIEVAL_HEALTH_ZERO_RESULT_THRESHOLD", threshold);
            std::env::set_var("DJINN_RETRIEVAL_HEALTH_QUERY_FLOOR", floor);
        }
    }

    #[test]
    fn from_env_uses_defaults() {
        let _guard = env_lock();
        clear_env_vars();
        let config = RetrievalHealthConfig::from_env().unwrap();
        assert_eq!(config.window_hours(), DEFAULT_WINDOW_HOURS);
        assert_eq!(
            config.zero_result_threshold(),
            DEFAULT_ZERO_RESULT_THRESHOLD
        );
        assert_eq!(config.query_floor(), DEFAULT_QUERY_FLOOR);
    }

    #[test]
    fn from_env_parses_custom_values() {
        let _guard = env_lock();
        set_env_vars("48", "0.75", "50");
        let config = RetrievalHealthConfig::from_env().unwrap();
        assert_eq!(config.window_hours(), 48);
        assert_eq!(config.zero_result_threshold(), 0.75);
        assert_eq!(config.query_floor(), 50);
    }

    #[test]
    fn from_env_rejects_invalid_window_hours() {
        let _guard = env_lock();
        set_env_vars("0", "0.5", "20");
        assert_eq!(
            RetrievalHealthConfig::from_env(),
            Err(RetrievalHealthConfigError::WindowHours(0))
        );
    }

    #[test]
    fn from_env_rejects_invalid_threshold() {
        let _guard = env_lock();
        set_env_vars("24", "1.1", "20");
        assert_eq!(
            RetrievalHealthConfig::from_env(),
            Err(RetrievalHealthConfigError::Threshold(1.1))
        );
    }

    #[test]
    fn from_env_rejects_invalid_query_floor() {
        let _guard = env_lock();
        set_env_vars("24", "0.5", "0");
        assert_eq!(
            RetrievalHealthConfig::from_env(),
            Err(RetrievalHealthConfigError::QueryFloor(0))
        );
    }

    #[test]
    fn from_env_rejects_non_numeric() {
        let _guard = env_lock();
        set_env_vars("not-a-number", "0.5", "20");
        assert!(
            matches!(
                RetrievalHealthConfig::from_env(),
                Err(RetrievalHealthConfigError::InvalidEnvValue { .. })
            ),
            "expected InvalidEnvValue for non-numeric env var"
        );
    }

    // -----------------------------------------------------------------
    // Floor and threshold behavior
    // -----------------------------------------------------------------

    #[test]
    fn at_floor_with_above_threshold_rate_emits() {
        // floor = 20, total = 20, zero = 11 -> rate 0.55 > 0.5
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let findings = run_check(config, vec![make_project_with_total_zero("p1", 20, 11)]);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn below_floor_does_not_emit() {
        // floor = 20, total = 19, zero = 19 -> rate 1.0 but below floor.
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let findings = run_check(config, vec![make_project_with_total_zero("p1", 19, 19)]);
        assert!(findings.is_empty());
    }

    #[test]
    fn threshold_equality_passes() {
        // floor = 20, total = 20, zero = 10 -> rate exactly 0.5, equality passes.
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let findings = run_check(config, vec![make_project_with_total_zero("p1", 20, 10)]);
        assert!(findings.is_empty());
    }

    #[test]
    fn above_threshold_emits() {
        // floor = 20, total = 20, zero = 11 -> rate 0.55 > 0.5.
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let findings = run_check(config, vec![make_project_with_total_zero("p1", 20, 11)]);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn below_threshold_does_not_emit() {
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let findings = run_check(config, vec![make_project_with_total_zero("p1", 20, 9)]);
        assert!(findings.is_empty());
    }

    #[test]
    fn zero_queries_does_not_emit() {
        let config = RetrievalHealthConfig::new(24, 0.5, 1).unwrap();
        let findings = run_check(config, vec![make_project_with_total_zero("p1", 0, 0)]);
        assert!(findings.is_empty());
    }

    // -----------------------------------------------------------------
    // Multiple-project isolation
    // -----------------------------------------------------------------

    #[test]
    fn only_above_threshold_project_yields_finding() {
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let projects = vec![
            make_project_with_total_zero("healthy", 100, 10), // rate 0.10
            make_project_with_total_zero("sick", 100, 60),    // rate 0.60
        ];
        let findings = run_check(config, projects);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].entity_ids.get("project_id").map(String::as_str),
            Some("sick")
        );
    }

    // -----------------------------------------------------------------
    // Evidence shape
    // -----------------------------------------------------------------

    #[test]
    fn evidence_contains_all_required_fields() {
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let projects = vec![make_project(
            "p-evidence",
            0,
            0,
            vec![("memory_search", 30, 18), ("memory_build_context", 20, 8)],
        )];
        let findings = run_check(config, projects);
        assert_eq!(findings.len(), 1);

        let f = &findings[0];
        assert_eq!(f.severity, FindingSeverity::Warn);
        assert_eq!(f.check_name, RETRIEVAL_ZERO_RESULT_NAME);
        assert_eq!(
            f.entity_ids.get("project_id").map(String::as_str),
            Some("p-evidence")
        );

        let evidence = &f.evidence;
        assert_eq!(evidence["project_id"], "p-evidence");
        assert!(evidence["window"].is_object());
        assert_eq!(evidence["threshold"], 0.5);
        assert_eq!(evidence["floor"], 20);
        assert_eq!(evidence["numerator"], 26);
        assert_eq!(evidence["denominator"], 50);
        assert!(evidence["rate"].is_number());
        assert!(evidence["per_entry_point_counts"].is_object());
        assert_eq!(
            evidence["per_entry_point_counts"]["memory_search"]["total_queries"],
            30
        );
        assert_eq!(
            evidence["per_entry_point_counts"]["memory_search"]["zero_result_queries"],
            18
        );
        assert_eq!(
            evidence["per_entry_point_counts"]["memory_build_context"]["total_queries"],
            20
        );
        assert_eq!(
            evidence["per_entry_point_counts"]["memory_build_context"]["zero_result_queries"],
            8
        );

        // Rate should be 26/50 = 0.52.
        let rate = evidence["rate"].as_f64().expect("rate is a number");
        assert!((rate - 0.52).abs() < f64::EPSILON);
    }

    #[test]
    fn resolver_snapshot_contains_inputs_and_outputs() {
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let projects = vec![make_project_with_total_zero("p-snap", 40, 22)];
        let findings = run_check(config, projects);
        assert_eq!(findings.len(), 1);

        let snap = &findings[0].resolver_snapshot;
        assert_eq!(snap.resolver, "resolve_retrieval_zero_result");
        assert_eq!(snap.inputs["project_id"], "p-snap");
        assert_eq!(snap.inputs["total_queries"], 40);
        assert_eq!(snap.inputs["zero_result_queries"], 22);
        assert_eq!(snap.outputs["above_threshold"], true);
        assert_eq!(snap.outputs["below_floor"], false);
    }

    // -----------------------------------------------------------------
    // Cadence
    // -----------------------------------------------------------------

    #[test]
    fn check_is_on_demand() {
        let config = RetrievalHealthConfig::default();
        let source: Arc<dyn RetrievalHealthDataSource> = Arc::new(MemoryDataSource::default());
        let check = RetrievalZeroResultCheck::new(config, source);
        assert_eq!(check.cadence(), DoctorCheckCadence::OnDemand);
    }

    // -----------------------------------------------------------------
    // Entry point aggregation
    // -----------------------------------------------------------------

    #[test]
    fn counts_aggregate_across_entry_points() {
        // Even though neither entry point alone exceeds threshold, the
        // aggregate does, and the total meets the floor.
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let projects = vec![make_project(
            "p-aggregate",
            0,
            0,
            vec![("memory_search", 20, 6), ("memory_build_context", 20, 15)],
        )];
        let findings = run_check(config, projects);
        assert_eq!(findings.len(), 1);
        let evidence = &findings[0].evidence;
        assert_eq!(evidence["numerator"], 21);
        assert_eq!(evidence["denominator"], 40);
    }

    #[test]
    fn healthy_project_aggregates_to_no_finding() {
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let projects = vec![make_project(
            "p-healthy",
            0,
            0,
            vec![("memory_search", 20, 5), ("memory_build_context", 20, 5)],
        )];
        let findings = run_check(config, projects);
        assert!(findings.is_empty());
    }
}
